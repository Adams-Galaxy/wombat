//! Read-only requirement checking against the current host environment.

use super::providers::*;
use super::*;

pub(super) fn check_context(context: &RequirementContext<'_>) -> Result<CheckOutcome> {
    let items = context
        .requirements
        .iter()
        .map(|requirement| check_requirement(context, requirement))
        .collect::<Result<Vec<_>>>()?;
    Ok(CheckOutcome {
        build_id: context.id.to_string(),
        items,
    })
}

pub(super) fn check_requirement(
    context: &RequirementContext<'_>,
    requirement: &Requirement,
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
                        requirement: label,
                        provider: requirement.binding.provider.clone(),
                        status: CheckStatus::Satisfied,
                        detail: format!("{} at {}", path.display(), observed),
                    });
                }
                return Ok(CheckItem {
                    requirement: label,
                    provider: requirement.binding.provider.clone(),
                    status: CheckStatus::Outdated,
                    detail: format!("observed {observed}; needs at least {minimum}"),
                });
            }
            return Ok(CheckItem {
                requirement: label,
                provider: requirement.binding.provider.clone(),
                status: CheckStatus::Satisfied,
                detail: path.display().to_string(),
            });
        }
    }
    let provider = provider_for(context.providers, &requirement.binding.provider)?;
    let mut result = match &provider.origin {
        ProviderOrigin::Builtin { .. } => match provider.name.as_str() {
            "brew" => check_brew(&requirement.binding, selected.minimum())?,
            "apt" => check_apt(&requirement.binding, selected.minimum())?,
            name => {
                return Err(WombatError::configuration(format!(
                    "unsupported built-in provider `{name}`"
                )));
            }
        },
        ProviderOrigin::Custom { .. } => check_custom(context, provider, requirement)?,
    };
    result.requirement = label;
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

pub(super) fn check_brew(binding: &ProviderBinding, minimum: Option<&str>) -> Result<CheckItem> {
    let (kind, name) = brew_identity(binding)?;
    let brew = which("brew");
    let Some(brew) = brew else {
        return Ok(provider_item(
            binding,
            CheckStatus::Unavailable,
            "Homebrew is not available on PATH",
        ));
    };
    let output = run_bounded(
        &brew,
        &["info", "--json=v2", brew_flag(kind), name],
        &BTreeMap::new(),
    )?;
    if !output.success {
        return Ok(provider_item(
            binding,
            CheckStatus::Unavailable,
            &format!("brew info failed: {}", output_detail(&output)),
        ));
    }
    let json: serde_json::Value = serde_json::from_slice(&output.stdout.bytes)?;
    let installed = installed_brew_versions(&json, kind)?;
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
        let Some((status, observed)) = text.trim().rsplit_once('\t') else {
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
        _ => Ok(provider_item(
            binding,
            CheckStatus::Unavailable,
            "no Apt candidate is available",
        )),
    }
}
