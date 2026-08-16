//! Read-only provider-prerequisite and requirement checks against the current
//! host environment.

use super::providers::*;
use super::*;

pub(super) fn check_context(context: &RequirementContext<'_>) -> Result<CheckOutcome> {
    let referenced = context
        .requirements
        .iter()
        .flat_map(|requirement| {
            requirement
                .binding
                .prerequisites
                .iter()
                .map(move |identity| (requirement.binding.provider.as_str(), identity.as_str()))
        })
        .collect::<BTreeSet<_>>();
    let mut items = context
        .prerequisites
        .iter()
        .filter(|prerequisite| {
            referenced.contains(&(
                prerequisite.provider.as_str(),
                prerequisite.identity.as_str(),
            ))
        })
        .map(|prerequisite| check_prerequisite(context, prerequisite))
        .collect::<Result<Vec<_>>>()?;
    // Each check is a blocking subprocess (brew, dpkg, git...), so the
    // dominant cost is wall-clock wait, not CPU — running them concurrently
    // turns N sequential spawns into roughly the slowest one.
    let brew = BrewSnapshot::fetch(context.requirements)?;
    let requirements = std::thread::scope(|scope| {
        context
            .requirements
            .iter()
            .map(|requirement| {
                // `WombatError` is not `Send` (it can wrap an `mlua::Error`,
                // which holds a non-Send `Arc<dyn Error>`), so cross the
                // thread boundary as a string and rebuild it after joining.
                scope.spawn(|| {
                    check_requirement(context, requirement, &brew)
                        .map_err(|error| error.to_string())
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
                    .map_err(WombatError::configuration)
            })
            .collect::<Result<Vec<_>>>()
    })?;
    items.extend(requirements);
    Ok(CheckOutcome {
        build_id: context.id.to_string(),
        items,
    })
}

pub(super) fn check_prerequisite(
    context: &RequirementContext<'_>,
    prerequisite: &ProviderPrerequisite,
) -> Result<CheckItem> {
    let started = std::time::Instant::now();
    let provider = provider_for(context.providers, &prerequisite.provider)?;
    let (status, detail) = match &provider.origin {
        ProviderOrigin::Builtin { .. } if provider.name == "apt" => {
            check_apt_source(context, prerequisite)?
        }
        ProviderOrigin::Builtin { .. } => {
            return Err(WombatError::configuration(format!(
                "built-in provider `{}` does not support prerequisites",
                provider.name
            )));
        }
        ProviderOrigin::Custom { .. } => {
            check_custom_prerequisite(context, provider, prerequisite)?
        }
    };
    Ok(CheckItem {
        subject: CheckSubject::Prerequisite,
        identity: format!(
            "prerequisite:{}:{}",
            prerequisite.provider, prerequisite.identity
        ),
        provider: prerequisite.provider.clone(),
        status,
        detail,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    })
}

pub(super) fn check_requirement(
    context: &RequirementContext<'_>,
    requirement: &Requirement,
    brew: &BrewSnapshot,
) -> Result<CheckItem> {
    let started = std::time::Instant::now();
    let mut item = check_requirement_uncounted(context, requirement, brew)?;
    item.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(item)
}

fn check_requirement_uncounted(
    context: &RequirementContext<'_>,
    requirement: &Requirement,
    brew: &BrewSnapshot,
) -> Result<CheckItem> {
    let selected = selected_candidate(requirement)?;
    let label = requirement_label(requirement);
    if requirement.kind == RequirementKind::Command {
        let command = selected.name();
        if let Some(path) = which(command) {
            if let Some(minimum) = selected.minimum() {
                let observed = observe_command_version(&path)?;
                if version_at_least(&observed, minimum) {
                    return Ok(CheckItem {
                        subject: CheckSubject::Requirement,
                        identity: label,
                        provider: requirement.binding.provider.clone(),
                        status: CheckStatus::Satisfied,
                        detail: format!("{} at {}", path.display(), observed),
                        duration_ms: 0,
                    });
                }
                return Ok(CheckItem {
                    subject: CheckSubject::Requirement,
                    identity: label,
                    provider: requirement.binding.provider.clone(),
                    status: CheckStatus::Outdated,
                    detail: format!("observed {observed}; needs at least {minimum}"),
                    duration_ms: 0,
                });
            }
            return Ok(CheckItem {
                subject: CheckSubject::Requirement,
                identity: label,
                provider: requirement.binding.provider.clone(),
                status: CheckStatus::Satisfied,
                detail: path.display().to_string(),
                duration_ms: 0,
            });
        }
    }
    let provider = provider_for(context.providers, &requirement.binding.provider)?;
    let mut result = match &provider.origin {
        ProviderOrigin::Builtin { .. } => match provider.name.as_str() {
            "brew" => check_brew(&requirement.binding, selected.minimum(), brew)?,
            "apt" => check_apt(&requirement.binding, selected.minimum())?,
            "git" => check_git(&requirement.binding)?,
            name => {
                return Err(WombatError::configuration(format!(
                    "unsupported built-in provider `{name}`"
                )));
            }
        },
        ProviderOrigin::Custom { .. } => check_custom(context, provider, requirement)?,
    };
    result.subject = CheckSubject::Requirement;
    result.identity = label;
    if result.status == CheckStatus::Satisfied {
        for command in &requirement.binding.publications.commands {
            if which(command).is_none() {
                result.status = CheckStatus::Missing;
                result.detail = format!(
                    "package is present but published command `{command}` is absent from PATH"
                );
                break;
            }
        }
    }
    Ok(result)
}

/// Batched Homebrew metadata for every `brew`-bound requirement in one check
/// pass. `brew info` cold-starts Ruby on every invocation, so querying every
/// requested formula and cask together turns N slow spawns into at most two.
pub(super) struct BrewSnapshot {
    brew: Option<PathBuf>,
    formulae: BTreeMap<String, serde_json::Value>,
    casks: BTreeMap<String, serde_json::Value>,
}

impl BrewSnapshot {
    pub(super) fn fetch(requirements: &[Requirement]) -> Result<Self> {
        let Some(brew) = which("brew") else {
            return Ok(Self {
                brew: None,
                formulae: BTreeMap::new(),
                casks: BTreeMap::new(),
            });
        };
        let mut formula_names = BTreeSet::new();
        let mut cask_names = BTreeSet::new();
        for requirement in requirements {
            if requirement.binding.provider != "brew" {
                continue;
            }
            // A command requirement never consults its provider once `which`
            // finds it on PATH — fetching brew metadata for it anyway would
            // reintroduce exactly the cold-start cost this snapshot exists to
            // avoid, for a check that was never going to touch brew.
            if requirement.kind == RequirementKind::Command
                && selected_candidate(requirement)
                    .is_ok_and(|selected| which(selected.name()).is_some())
            {
                continue;
            }
            let Ok((kind, name)) = brew_identity(&requirement.binding) else {
                continue;
            };
            if kind == "cask" {
                cask_names.insert(name.to_string());
            } else {
                formula_names.insert(name.to_string());
            }
        }
        Ok(Self {
            formulae: Self::fetch_records(&brew, "--formula", "formulae", "name", &formula_names)?,
            casks: Self::fetch_records(&brew, "--cask", "casks", "token", &cask_names)?,
            brew: Some(brew),
        })
    }

    /// One `brew info` call for every name of a kind. A single unresolvable
    /// name fails the whole call with no stdout, so a failure here leaves the
    /// snapshot empty for that kind rather than erroring — the per-item
    /// fallback in `check_brew` then re-queries individually and reports
    /// exactly which name is at fault.
    fn fetch_records(
        brew: &Path,
        flag: &str,
        array_key: &str,
        name_field: &str,
        names: &BTreeSet<String>,
    ) -> Result<BTreeMap<String, serde_json::Value>> {
        if names.is_empty() {
            return Ok(BTreeMap::new());
        }
        let mut args = vec!["info", "--json=v2", flag];
        args.extend(names.iter().map(String::as_str));
        let output = run_bounded(brew, &args, &brew_environment())?;
        if !output.success {
            return Ok(BTreeMap::new());
        }
        let json: serde_json::Value = serde_json::from_slice(&output.stdout.bytes)?;
        let mut records = BTreeMap::new();
        if let Some(array) = json.get(array_key).and_then(serde_json::Value::as_array) {
            for entry in array {
                if let Some(name) = entry.get(name_field).and_then(serde_json::Value::as_str) {
                    records.insert(name.to_string(), entry.clone());
                }
            }
        }
        Ok(records)
    }
}

pub(super) fn check_brew(
    binding: &ProviderBinding,
    minimum: Option<&str>,
    snapshot: &BrewSnapshot,
) -> Result<CheckItem> {
    let (kind, name) = brew_identity(binding)?;
    let Some(brew) = &snapshot.brew else {
        return Ok(provider_item(
            binding,
            CheckStatus::Unavailable,
            "Homebrew is not available on PATH",
        ));
    };
    let cached = if kind == "cask" {
        snapshot.casks.get(name)
    } else {
        snapshot.formulae.get(name)
    };
    let entry = match cached {
        Some(entry) => entry.clone(),
        None => {
            let output = run_bounded(
                brew,
                &["info", "--json=v2", brew_flag(kind), name],
                &brew_environment(),
            )?;
            if !output.success {
                return Ok(provider_item(
                    binding,
                    CheckStatus::Unavailable,
                    &format!("brew info failed: {}", output_detail(&output)),
                ));
            }
            let json: serde_json::Value = serde_json::from_slice(&output.stdout.bytes)?;
            let array_key = if kind == "cask" { "casks" } else { "formulae" };
            let Some(entry) = json
                .get(array_key)
                .and_then(serde_json::Value::as_array)
                .and_then(|values| values.first())
            else {
                return Err(WombatError::configuration(
                    "Homebrew returned no matching package record",
                ));
            };
            entry.clone()
        }
    };
    let installed = installed_brew_versions(&entry);
    let Some(observed) = installed.last() else {
        return Ok(provider_item(
            binding,
            CheckStatus::Missing,
            "not installed",
        ));
    };
    if let Some(minimum) = minimum
        && !version_at_least(observed, minimum)
    {
        return Ok(provider_item(
            binding,
            CheckStatus::Outdated,
            &format!("observed {observed}; needs at least {minimum}"),
        ));
    }
    Ok(provider_item(
        binding,
        CheckStatus::Satisfied,
        &format!("{kind} {name} {observed}"),
    ))
}

pub(super) fn check_apt(binding: &ProviderBinding, minimum: Option<&str>) -> Result<CheckItem> {
    let name = apt_identity(binding)?;
    let Some(dpkg_query) = which("dpkg-query") else {
        return Ok(provider_item(
            binding,
            CheckStatus::Unavailable,
            "dpkg-query is not available on PATH",
        ));
    };
    let output = run_bounded(
        &dpkg_query,
        &["-W", "-f=${Status}\t${Version}", name],
        &BTreeMap::new(),
    )?;
    if output.success {
        let text = String::from_utf8_lossy(&output.stdout.bytes);
        let Some((status, observed)) = parse_dpkg_record(&text) else {
            return Ok(provider_item(
                binding,
                CheckStatus::Unavailable,
                "dpkg-query returned an unrecognized package record",
            ));
        };
        if status != "install ok installed" {
            return Ok(provider_item(
                binding,
                CheckStatus::Missing,
                &format!("dpkg status is {status}"),
            ));
        }
        if let Some(minimum) = minimum {
            let Some(dpkg) = which("dpkg") else {
                return Ok(provider_item(
                    binding,
                    CheckStatus::Unavailable,
                    "dpkg is unavailable for Debian version comparison",
                ));
            };
            let comparison = run_bounded(
                &dpkg,
                &["--compare-versions", observed, "ge", minimum],
                &BTreeMap::new(),
            )?;
            if !comparison.success {
                return Ok(provider_item(
                    binding,
                    CheckStatus::Outdated,
                    &format!("observed {observed}; needs at least {minimum}"),
                ));
            }
        }
        return Ok(provider_item(
            binding,
            CheckStatus::Satisfied,
            &format!("package {name} {observed}"),
        ));
    }

    let Some(apt_cache) = which("apt-cache") else {
        return Ok(provider_item(
            binding,
            CheckStatus::Unavailable,
            "apt-cache is not available on PATH",
        ));
    };
    let policy = run_bounded(&apt_cache, &["policy", name], &BTreeMap::new())?;
    if !policy.success {
        return Ok(provider_item(
            binding,
            CheckStatus::Unavailable,
            &format!("apt-cache policy failed: {}", output_detail(&policy)),
        ));
    }
    let policy_text = String::from_utf8_lossy(&policy.stdout.bytes);
    let candidate = policy_text
        .lines()
        .find_map(|line| line.trim().strip_prefix("Candidate:").map(str::trim));
    match candidate {
        Some(candidate) if candidate != "(none)" => Ok(provider_item(
            binding,
            CheckStatus::Missing,
            &format!("not installed; candidate {candidate}"),
        )),
        _ if !binding.prerequisites.is_empty() => Ok(provider_item(
            binding,
            CheckStatus::Missing,
            "not installed; declared Apt source will provide the candidate",
        )),
        _ => Ok(provider_item(
            binding,
            CheckStatus::Unavailable,
            "no Apt candidate is available",
        )),
    }
}

fn parse_dpkg_record(text: &str) -> Option<(&str, &str)> {
    // An uninstalled package has an empty `${Version}`, so the trailing tab is
    // the only evidence that dpkg-query returned both requested fields.
    text.trim_end_matches(['\r', '\n']).rsplit_once('\t')
}

pub(super) fn check_git(binding: &ProviderBinding) -> Result<CheckItem> {
    let (repository, to, reference) = git_identity(binding)?;
    let Some(git) = which("git") else {
        return Ok(provider_item(
            binding,
            CheckStatus::Unavailable,
            "git is not available on PATH",
        ));
    };
    if !Path::new(to).join(".git").is_dir() {
        return Ok(provider_item(
            binding,
            CheckStatus::Missing,
            &format!("{to} is not a git checkout"),
        ));
    }
    // An existing checkout with no `origin` at all (or a different one) is the
    // same "wrong thing is there" outcome as a mismatched remote, not an
    // inconclusive one — treat both as outdated rather than unavailable.
    let remote = run_bounded(
        &git,
        &["-C", to, "remote", "get-url", "origin"],
        &BTreeMap::new(),
    )?;
    let observed_remote = String::from_utf8_lossy(&remote.stdout.bytes)
        .trim()
        .to_string();
    if !remote.success || observed_remote != repository {
        return Ok(provider_item(
            binding,
            CheckStatus::Outdated,
            &format!(
                "checkout remote is {}, expected {repository}",
                if remote.success {
                    observed_remote.as_str()
                } else {
                    "unset"
                }
            ),
        ));
    }
    let Some(reference) = reference else {
        return Ok(provider_item(
            binding,
            CheckStatus::Satisfied,
            &format!("checked out at {to}"),
        ));
    };
    // Resolved locally, not against `origin`, so a satisfied check never
    // touches the network: `reconcile` already fetched every ref it pinned.
    let wanted = run_bounded(
        &git,
        &[
            "-C",
            to,
            "rev-parse",
            "--verify",
            &format!("{reference}^{{commit}}"),
        ],
        &BTreeMap::new(),
    )?;
    if !wanted.success {
        return Ok(provider_item(
            binding,
            CheckStatus::Outdated,
            &format!("ref `{reference}` is not resolvable locally; a fetch is needed"),
        ));
    }
    let head = run_bounded(&git, &["-C", to, "rev-parse", "HEAD"], &BTreeMap::new())?;
    if !head.success {
        return Ok(provider_item(
            binding,
            CheckStatus::Unavailable,
            &format!("git rev-parse HEAD failed: {}", output_detail(&head)),
        ));
    }
    let wanted_commit = String::from_utf8_lossy(&wanted.stdout.bytes)
        .trim()
        .to_string();
    let head_commit = String::from_utf8_lossy(&head.stdout.bytes)
        .trim()
        .to_string();
    if wanted_commit != head_commit {
        return Ok(provider_item(
            binding,
            CheckStatus::Outdated,
            &format!("checked out at {head_commit}, expected {reference} ({wanted_commit})"),
        ));
    }
    Ok(provider_item(
        binding,
        CheckStatus::Satisfied,
        &format!("checked out {reference} at {to}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::parse_dpkg_record;

    #[test]
    fn dpkg_records_preserve_empty_versions_for_uninstalled_packages() {
        assert_eq!(
            parse_dpkg_record("unknown ok not-installed\t"),
            Some(("unknown ok not-installed", ""))
        );
        assert_eq!(
            parse_dpkg_record("unknown ok not-installed\t\n"),
            Some(("unknown ok not-installed", ""))
        );
        assert_eq!(
            parse_dpkg_record("unknown ok not-installed\t\r\n"),
            Some(("unknown ok not-installed", ""))
        );
    }

    #[test]
    fn dpkg_records_keep_installed_versions_and_reject_missing_delimiters() {
        assert_eq!(
            parse_dpkg_record("install ok installed\t1.2.3-1\n"),
            Some(("install ok installed", "1.2.3-1"))
        );
        assert_eq!(parse_dpkg_record("unknown ok not-installed\n"), None);
    }
}
