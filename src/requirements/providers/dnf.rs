//! DNF package checks and the narrowly managed RPM Fusion prerequisite.

use super::*;

#[derive(Clone, Debug, Eq, PartialEq)]
struct RpmFusion {
    kind: String,
    major: u64,
    package: String,
    url: String,
}

pub(super) fn dnf_identity(binding: &ProviderBinding) -> Result<&str> {
    let FrozenValue::Map(data) = &binding.data else {
        return Err(WombatError::configuration("DNF binding data must be a map"));
    };
    match data.get("name") {
        Some(FrozenValue::String(value))
            if value
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_alphanumeric())
                && value.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'.' | b'_' | b'-')
                }) =>
        {
            Ok(value)
        }
        Some(FrozenValue::String(_)) => Err(WombatError::configuration(
            "DNF binding package name is not a safe package token",
        )),
        _ => Err(WombatError::configuration("DNF binding lacks package name")),
    }
}

fn dnf_command(context: &RequirementContext<'_>, purpose: &str) -> Result<PathBuf> {
    context.command("dnf").ok_or_else(|| {
        WombatError::configuration(format!("{purpose} requires `dnf` to be available on PATH"))
    })
}

fn ensure_mutable_fedora(context: &RequirementContext<'_>) -> Result<()> {
    if context.system_root.join("run/ostree-booted").exists() {
        return Err(WombatError::configuration(
            "DNF package mutation is not supported on Atomic Fedora; use Flatpak or manage rpm-ostree/bootc layering outside Wombat",
        ));
    }
    Ok(())
}

pub(crate) fn check_dnf(
    context: &RequirementContext<'_>,
    binding: &ProviderBinding,
    minimum: Option<&str>,
) -> Result<CheckItem> {
    if context.system_root.join("run/ostree-booted").exists() {
        return Ok(provider_item(
            binding,
            CheckStatus::Unavailable,
            "DNF package mutation is unsupported on Atomic Fedora",
        ));
    }
    let name = dnf_identity(binding)?;
    let Some(rpm) = context.command("rpm") else {
        return Ok(provider_item(
            binding,
            CheckStatus::Unavailable,
            "RPM is not available on PATH",
        ));
    };
    let installed = run_bounded(
        &rpm,
        &["-q", "--qf", "%{EVR}\\n", "--", name],
        &BTreeMap::new(),
    )?;
    if installed.success {
        let observed = String::from_utf8_lossy(&installed.stdout.bytes)
            .trim()
            .to_string();
        if observed.is_empty() {
            return Ok(provider_item(
                binding,
                CheckStatus::Unavailable,
                "RPM returned an empty installed version",
            ));
        }
        if let Some(minimum) = minimum
            && !version_at_least(&observed, minimum)
        {
            return Ok(provider_item(
                binding,
                CheckStatus::Outdated,
                &format!("observed {observed}; needs at least {minimum}"),
            ));
        }
        return Ok(provider_item(
            binding,
            CheckStatus::Satisfied,
            &format!("package {name} {observed}"),
        ));
    }
    if !binding.prerequisites.is_empty() {
        return Ok(provider_item(
            binding,
            CheckStatus::Missing,
            "not installed; declared RPM Fusion prerequisite will provide the package",
        ));
    }
    let Some(dnf) = context.command("dnf") else {
        return Ok(provider_item(
            binding,
            CheckStatus::Unavailable,
            "DNF5 is not available on PATH",
        ));
    };
    let available = run_bounded(
        &dnf,
        &[
            "repoquery",
            "--available",
            "--queryformat",
            "%{name}\\n",
            name,
        ],
        &BTreeMap::new(),
    )?;
    if !available.success {
        return Ok(provider_item(
            binding,
            CheckStatus::Unavailable,
            &format!("dnf repoquery failed: {}", output_detail(&available)),
        ));
    }
    if String::from_utf8_lossy(&available.stdout.bytes)
        .lines()
        .any(|line| line.trim() == name)
    {
        Ok(provider_item(
            binding,
            CheckStatus::Missing,
            "not installed; a DNF candidate is available",
        ))
    } else {
        Ok(provider_item(
            binding,
            CheckStatus::Unavailable,
            "no DNF candidate is available",
        ))
    }
}

pub(super) fn preflight_dnf_requirement(
    context: &RequirementContext<'_>,
    requirement: &Requirement,
) -> Result<()> {
    ensure_mutable_fedora(context)?;
    context.command("rpm").ok_or_else(|| {
        WombatError::configuration("DNF package observation requires `rpm` to be available on PATH")
    })?;
    let dnf = dnf_command(context, "DNF package reconciliation")?;
    let name = dnf_identity(&requirement.binding)?;
    let available = run_bounded(
        &dnf,
        &[
            "repoquery",
            "--available",
            "--queryformat",
            "%{name}\\n",
            name,
        ],
        &BTreeMap::new(),
    )?;
    if !available.success
        || !String::from_utf8_lossy(&available.stdout.bytes)
            .lines()
            .any(|line| line.trim() == name)
    {
        return Err(WombatError::configuration(format!(
            "DNF preflight found no available candidate for `{name}`: {}",
            output_detail(&available)
        )));
    }
    preflight_elevation(requirement.binding.elevated && context.system_root == Path::new("/"))
}

pub(super) fn reconcile_dnf_requirement(
    context: &RequirementContext<'_>,
    requirement: &Requirement,
    noninteractive: bool,
) -> Result<()> {
    ensure_mutable_fedora(context)?;
    let name = dnf_identity(&requirement.binding)?;
    let dnf = dnf_command(context, "DNF package reconciliation")?;
    run_mutating(
        &dnf,
        &["install", "--assumeyes", name],
        &BTreeMap::new(),
        requirement.binding.elevated && context.system_root == Path::new("/"),
        noninteractive,
    )
}

fn rpmfusion(prerequisite: &ProviderPrerequisite) -> Result<RpmFusion> {
    let FrozenValue::Map(data) = &prerequisite.data else {
        return Err(WombatError::configuration(
            "RPM Fusion prerequisite data must be a map",
        ));
    };
    if data
        .keys()
        .any(|field| !matches!(field.as_str(), "kind" | "major" | "package" | "url"))
    {
        return Err(WombatError::configuration(
            "RPM Fusion prerequisite contains an unsupported field",
        ));
    }
    let string = |field: &str| match data.get(field) {
        Some(FrozenValue::String(value)) => Ok(value.clone()),
        _ => Err(WombatError::configuration(format!(
            "RPM Fusion prerequisite lacks `{field}`"
        ))),
    };
    let major = match data.get("major") {
        Some(FrozenValue::Integer(value)) if *value > 0 => u64::try_from(*value)
            .map_err(|_| WombatError::configuration("RPM Fusion Fedora major is invalid"))?,
        _ => {
            return Err(WombatError::configuration(
                "RPM Fusion prerequisite lacks a numeric Fedora major",
            ));
        }
    };
    let value = RpmFusion {
        kind: string("kind")?,
        major,
        package: string("package")?,
        url: string("url")?,
    };
    if !matches!(value.kind.as_str(), "free" | "nonfree")
        || value.package != format!("rpmfusion-{}-release", value.kind)
        || prerequisite.identity != format!("repository:rpmfusion-{}", value.kind)
        || value.url
            != format!(
                "https://mirrors.rpmfusion.org/{}/fedora/rpmfusion-{}-release-{}.noarch.rpm",
                value.kind, value.kind, value.major
            )
        || !prerequisite.elevated
    {
        return Err(WombatError::configuration(
            "RPM Fusion prerequisite contract is inconsistent",
        ));
    }
    Ok(value)
}

pub(crate) fn check_rpmfusion(
    context: &RequirementContext<'_>,
    prerequisite: &ProviderPrerequisite,
) -> Result<(CheckStatus, String)> {
    let repository = rpmfusion(prerequisite)?;
    let Some(rpm) = context.command("rpm") else {
        return Ok((
            CheckStatus::Unavailable,
            "RPM is not available on PATH".to_string(),
        ));
    };
    let output = run_bounded(
        &rpm,
        &["-q", "--qf", "%{VERSION}\\n", "--", &repository.package],
        &BTreeMap::new(),
    )?;
    if !output.success {
        return Ok((
            CheckStatus::Missing,
            format!("{} is not installed", repository.package),
        ));
    }
    let observed = String::from_utf8_lossy(&output.stdout.bytes)
        .trim()
        .to_string();
    if observed == repository.major.to_string() {
        Ok((
            CheckStatus::Satisfied,
            format!("{} targets Fedora {}", repository.package, repository.major),
        ))
    } else {
        Ok((
            CheckStatus::Outdated,
            format!(
                "{} targets Fedora {observed}, expected {}",
                repository.package, repository.major
            ),
        ))
    }
}

pub(super) fn preflight_rpmfusion(
    context: &RequirementContext<'_>,
    prerequisite: &ProviderPrerequisite,
) -> Result<()> {
    ensure_mutable_fedora(context)?;
    rpmfusion(prerequisite)?;
    context.command("rpm").ok_or_else(|| {
        WombatError::configuration("RPM Fusion observation requires `rpm` to be available on PATH")
    })?;
    dnf_command(context, "RPM Fusion reconciliation")?;
    preflight_elevation(prerequisite.elevated && context.system_root == Path::new("/"))
}

pub(super) fn reconcile_rpmfusion(
    context: &RequirementContext<'_>,
    prerequisite: &ProviderPrerequisite,
    noninteractive: bool,
) -> Result<()> {
    ensure_mutable_fedora(context)?;
    let repository = rpmfusion(prerequisite)?;
    let dnf = dnf_command(context, "RPM Fusion reconciliation")?;
    run_mutating(
        &dnf,
        &["install", "--assumeyes", &repository.url],
        &BTreeMap::new(),
        prerequisite.elevated && context.system_root == Path::new("/"),
        noninteractive,
    )
}

pub(super) fn validate_dnf_contract(
    requirements: &[Requirement],
    prerequisites: &[ProviderPrerequisite],
) -> Result<()> {
    let repositories = prerequisites
        .iter()
        .filter(|value| value.provider == BuiltinProvider::Dnf.name())
        .map(|value| Ok((value.identity.as_str(), rpmfusion(value)?)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    for requirement in requirements
        .iter()
        .filter(|value| value.binding.provider == BuiltinProvider::Dnf.name())
    {
        let binding = &requirement.binding;
        if !binding.elevated {
            return Err(WombatError::configuration(
                "DNF package bindings must declare elevation",
            ));
        }
        let name = dnf_identity(binding)?;
        if binding.identity != format!("package:{name}") {
            return Err(WombatError::configuration(
                "DNF binding identity does not match its package name",
            ));
        }
        let FrozenValue::Map(data) = &binding.data else {
            unreachable!("dnf_identity validates map data")
        };
        if data
            .keys()
            .any(|field| !matches!(field.as_str(), "name" | "rpmfusion"))
        {
            return Err(WombatError::configuration(
                "DNF binding contains an unsupported field",
            ));
        }
        match data.get("rpmfusion") {
            None if binding.prerequisites.is_empty() => {}
            Some(FrozenValue::String(kind)) if kind == "free" => {
                if binding.prerequisites != ["repository:rpmfusion-free"] {
                    return Err(WombatError::configuration(
                        "DNF RPM Fusion free binding has inconsistent prerequisites",
                    ));
                }
            }
            Some(FrozenValue::String(kind)) if kind == "nonfree" => {
                if binding.prerequisites
                    != ["repository:rpmfusion-free", "repository:rpmfusion-nonfree"]
                {
                    return Err(WombatError::configuration(
                        "DNF RPM Fusion nonfree binding has inconsistent prerequisites",
                    ));
                }
            }
            _ => {
                return Err(WombatError::configuration(
                    "DNF binding has invalid RPM Fusion policy",
                ));
            }
        }
        if binding
            .prerequisites
            .iter()
            .any(|identity| !repositories.contains_key(identity.as_str()))
        {
            return Err(WombatError::configuration(
                "DNF binding references an invalid RPM Fusion prerequisite",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn executable(path: &Path, source: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        let staged = path.with_extension("wombat-test-staged");
        fs::write(&staged, source).unwrap();
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755)).unwrap();
        fs::rename(staged, path).unwrap();
    }

    fn binding(prerequisites: Vec<String>) -> ProviderBinding {
        ProviderBinding {
            provider: "dnf".to_string(),
            identity: "package:ffmpeg".to_string(),
            elevated: true,
            package: Some("ffmpeg".to_string()),
            publications: crate::model::manifest::Publications {
                commands: Vec::new(),
            },
            prerequisites,
            data: serde_json::from_value(serde_json::json!({
                "name": "ffmpeg",
                "rpmfusion": "free",
            }))
            .unwrap(),
        }
    }

    fn requirement(binding: ProviderBinding) -> Requirement {
        Requirement {
            kind: RequirementKind::Package,
            owner: "root".to_string(),
            declared_at: crate::model::manifest::SourceTrace {
                primary: crate::model::manifest::SourceLocation {
                    source: "wombat.lua".to_string(),
                    line: Some(1),
                    column: None,
                },
                callers: Vec::new(),
            },
            candidates: vec![RequirementCandidate::Package {
                name: "ffmpeg".to_string(),
                provider: Some("dnf".to_string()),
                minimum: None,
                publications: crate::model::manifest::Publications {
                    commands: Vec::new(),
                },
                with: FrozenValue::empty_map(),
            }],
            attempts: vec![crate::model::manifest::ResolutionAttempt {
                candidate: 0,
                provider: "dnf".to_string(),
                outcome: crate::model::manifest::ResolutionOutcome::Selected,
            }],
            selected: 0,
            choice: crate::model::manifest::RequirementChoice::Required,
            binding,
            when: crate::execution::ladder::CoreRung::MaterialiseBefore.into(),
        }
    }

    fn prerequisite() -> ProviderPrerequisite {
        ProviderPrerequisite {
            provider: "dnf".to_string(),
            identity: "repository:rpmfusion-free".to_string(),
            description: "Configure RPM Fusion free for Fedora 44".to_string(),
            when: crate::execution::ladder::CoreRung::MaterialiseBefore.into(),
            elevated: true,
            data: serde_json::from_value(serde_json::json!({
                "kind": "free",
                "major": 44,
                "package": "rpmfusion-free-release",
                "url": "https://mirrors.rpmfusion.org/free/fedora/rpmfusion-free-release-44.noarch.rpm",
            }))
            .unwrap(),
        }
    }

    #[test]
    #[cfg(unix)]
    fn rpm_fusion_and_package_reconcile_in_frozen_order() {
        let temporary = tempfile::tempdir().unwrap();
        let bin = temporary.path().join("bin");
        let state = temporary.path().join("state");
        let invocations = temporary.path().join("dnf-invocations");
        fs::create_dir(&bin).unwrap();
        let release = state.with_extension("release");
        let package = state.with_extension("package");
        executable(
            &bin.join("rpm"),
            &format!(
                "#!/bin/sh\ncase \"$*\" in\n  *rpmfusion-free-release*) [ -f '{}' ] && printf '44\\n' && exit 0;;\n  *ffmpeg*) [ -f '{}' ] && printf '1.2.3-1\\n' && exit 0;;\nesac\nexit 1\n",
                release.display(),
                package.display()
            ),
        );
        executable(
            &bin.join("dnf"),
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$*\" in\n  *repoquery*) printf 'ffmpeg\\n';;\n  *rpmfusion-free-release-44.noarch.rpm*) : > '{}' ;;\n  *ffmpeg*) : > '{}' ;;\nesac\nexit 0\n",
                invocations.display(),
                release.display(),
                package.display()
            ),
        );
        let prerequisite = prerequisite();
        let requirement = requirement(binding(vec![prerequisite.identity.clone()]));
        let context = RequirementContext {
            id: "fixture",
            providers: &[],
            requirements: std::slice::from_ref(&requirement),
            prerequisites: std::slice::from_ref(&prerequisite),
            preparations: &[],
            ladder: crate::execution::ladder::ExecutionLadder::default(),
            payload_root: temporary.path().join("payloads"),
            system_root: temporary.path().join("root"),
            command_root: Some(bin),
        };

        assert_eq!(
            check_rpmfusion(&context, &prerequisite).unwrap().0,
            CheckStatus::Missing
        );
        reconcile_rpmfusion(&context, &prerequisite, true).unwrap();
        assert_eq!(
            check_rpmfusion(&context, &prerequisite).unwrap().0,
            CheckStatus::Satisfied
        );
        preflight_dnf_requirement(&context, &requirement).unwrap();
        reconcile_dnf_requirement(&context, &requirement, true).unwrap();
        let invocations = fs::read_to_string(invocations).unwrap();
        assert!(
            invocations.lines().any(|line| line
                == "install --assumeyes https://mirrors.rpmfusion.org/free/fedora/rpmfusion-free-release-44.noarch.rpm"),
            "{invocations}"
        );
        assert!(
            invocations
                .lines()
                .any(|line| line == "install --assumeyes ffmpeg"),
            "{invocations}"
        );
        assert_eq!(
            check_dnf(&context, &requirement.binding, None)
                .unwrap()
                .status,
            CheckStatus::Satisfied
        );
    }

    #[test]
    #[cfg(unix)]
    fn atomic_fedora_rejects_dnf_before_mutation() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("root");
        fs::create_dir_all(root.join("run/ostree-booted")).unwrap();
        let requirement = requirement(binding(Vec::new()));
        let context = RequirementContext {
            id: "fixture",
            providers: &[],
            requirements: std::slice::from_ref(&requirement),
            prerequisites: &[],
            preparations: &[],
            ladder: crate::execution::ladder::ExecutionLadder::default(),
            payload_root: temporary.path().join("payloads"),
            system_root: root,
            command_root: Some(temporary.path().join("bin")),
        };
        let error = preflight_dnf_requirement(&context, &requirement)
            .unwrap_err()
            .to_string();
        assert!(error.contains("Atomic Fedora"), "{error}");
    }

    #[test]
    #[cfg(unix)]
    fn dnf_checks_installed_versions_and_repository_availability() {
        let temporary = tempfile::tempdir().unwrap();
        let bin = temporary.path().join("bin");
        let installed = temporary.path().join("installed");
        let available = temporary.path().join("available");
        fs::create_dir(&bin).unwrap();
        executable(
            &bin.join("rpm"),
            &format!(
                "#!/bin/sh\n[ -f '{}' ] || exit 1\ncat '{}'\n",
                installed.display(),
                installed.display()
            ),
        );
        executable(
            &bin.join("dnf"),
            &format!(
                "#!/bin/sh\n[ -f '{}' ] && printf 'ffmpeg\\n'\n",
                available.display()
            ),
        );
        let mut binding = binding(Vec::new());
        binding.data = serde_json::from_value(serde_json::json!({ "name": "ffmpeg" })).unwrap();
        let context = RequirementContext {
            id: "fixture",
            providers: &[],
            requirements: &[],
            prerequisites: &[],
            preparations: &[],
            ladder: crate::execution::ladder::ExecutionLadder::default(),
            payload_root: temporary.path().join("payloads"),
            system_root: temporary.path().join("root"),
            command_root: Some(bin),
        };

        assert_eq!(
            check_dnf(&context, &binding, None).unwrap().status,
            CheckStatus::Unavailable
        );
        fs::write(&available, "yes").unwrap();
        assert_eq!(
            check_dnf(&context, &binding, None).unwrap().status,
            CheckStatus::Missing
        );
        fs::write(&installed, "1.2.3-1\n").unwrap();
        assert_eq!(
            check_dnf(&context, &binding, Some("2.0")).unwrap().status,
            CheckStatus::Outdated
        );
        assert_eq!(
            check_dnf(&context, &binding, Some("1.2")).unwrap().status,
            CheckStatus::Satisfied
        );
    }

    #[test]
    #[cfg(unix)]
    fn rpm_fusion_reports_major_drift() {
        let temporary = tempfile::tempdir().unwrap();
        let bin = temporary.path().join("bin");
        fs::create_dir(&bin).unwrap();
        executable(&bin.join("rpm"), "#!/bin/sh\nprintf '43\\n'\n");
        let prerequisite = prerequisite();
        let context = RequirementContext {
            id: "fixture",
            providers: &[],
            requirements: &[],
            prerequisites: std::slice::from_ref(&prerequisite),
            preparations: &[],
            ladder: crate::execution::ladder::ExecutionLadder::default(),
            payload_root: temporary.path().join("payloads"),
            system_root: temporary.path().join("root"),
            command_root: Some(bin),
        };
        let (status, detail) = check_rpmfusion(&context, &prerequisite).unwrap();
        assert_eq!(status, CheckStatus::Outdated);
        assert!(detail.contains("expected 44"), "{detail}");
    }
}
