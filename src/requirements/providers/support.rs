//! Shared provider lookup, process, elevation, and version support.

use super::*;

pub(crate) fn ensure_compatible_host(manifest: &Manifest) -> Result<()> {
    ensure_compatible_platform(&manifest.target.platform)
}

pub(crate) fn ensure_compatible_platform(
    platform: &crate::model::context::TargetPlatform,
) -> Result<()> {
    let host = HostContext::observe()?;
    if !platform.locally_compatible_with(&host.platform) {
        return Err(WombatError::configuration(format!(
            "requirements target {}, but this execution environment is {}; check and bootstrap require an exact local OS and architecture",
            platform.compact(),
            host.platform.compact()
        )));
    }
    Ok(())
}

pub(crate) fn selected_candidate(requirement: &Requirement) -> Result<&RequirementCandidate> {
    requirement
        .candidates
        .get(requirement.selected as usize)
        .ok_or_else(|| {
            WombatError::configuration("requirement selection is outside its candidates")
        })
}

pub(crate) fn requirement_label(requirement: &Requirement) -> String {
    format!(
        "{}:{}",
        match requirement.kind {
            RequirementKind::Command => "command",
            RequirementKind::Package => "package",
        },
        requirement.candidates[requirement.selected as usize].name()
    )
}

pub(crate) fn provider_for<'a>(providers: &'a [Provider], name: &str) -> Result<&'a Provider> {
    providers
        .iter()
        .find(|provider| provider.name == name)
        .ok_or_else(|| WombatError::configuration(format!("selected provider `{name}` is absent")))
}

pub(crate) fn requirement_for_item<'a>(
    context: &'a RequirementContext<'_>,
    item: &CheckItem,
) -> Result<&'a Requirement> {
    context
        .requirements
        .iter()
        .find(|requirement| requirement_label(requirement) == item.identity)
        .ok_or_else(|| WombatError::configuration("check item references an absent requirement"))
}

pub(crate) fn prerequisite_for_item<'a>(
    context: &'a RequirementContext<'_>,
    item: &CheckItem,
) -> Result<&'a ProviderPrerequisite> {
    context
        .prerequisites
        .iter()
        .find(|prerequisite| {
            item.identity
                == format!(
                    "prerequisite:{}:{}",
                    prerequisite.provider, prerequisite.identity
                )
        })
        .ok_or_else(|| WombatError::configuration("check item references an absent prerequisite"))
}

pub(crate) fn provider_item(
    binding: &ProviderBinding,
    status: CheckStatus,
    detail: &str,
) -> CheckItem {
    CheckItem {
        subject: CheckSubject::Requirement,
        identity: String::new(),
        provider: binding.provider.clone(),
        status,
        detail: detail.to_string(),
        duration_ms: 0,
    }
}

pub(crate) fn frozen_binding(binding: &ProviderBinding) -> Result<FrozenValue> {
    Ok(serde_json::from_value(serde_json::to_value(binding)?)?)
}

pub(crate) fn frozen_preparation(preparation: &ProviderPreparation) -> Result<FrozenValue> {
    Ok(serde_json::from_value(serde_json::to_value(preparation)?)?)
}

pub(crate) fn frozen_prerequisite(prerequisite: &ProviderPrerequisite) -> Result<FrozenValue> {
    Ok(serde_json::from_value(serde_json::to_value(prerequisite)?)?)
}

pub(crate) fn require_command(command: &str, purpose: &str) -> Result<PathBuf> {
    which(command).ok_or_else(|| {
        WombatError::configuration(format!(
            "{purpose} requires `{command}` to be available on PATH"
        ))
    })
}

pub(crate) fn effective_uid_is_root() -> Result<bool> {
    let id = which("id").unwrap_or_else(|| PathBuf::from("/usr/bin/id"));
    let output = run_bounded(&id, &["-u"], &BTreeMap::new())?;
    if !output.success {
        return Err(WombatError::configuration(format!(
            "could not determine the effective user: {}",
            output_detail(&output)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout.bytes).trim() == "0")
}

pub(crate) fn preflight_elevation(elevated: bool) -> Result<()> {
    if elevated && !effective_uid_is_root()? {
        require_command("sudo", "elevated bootstrap")?;
    }
    Ok(())
}

pub(crate) fn authorize_elevation(noninteractive: bool) -> Result<()> {
    if effective_uid_is_root()? {
        return Ok(());
    }
    let sudo = require_command("sudo", "elevated bootstrap")?;
    let mut command = Command::new(&sudo);
    if noninteractive {
        command.args(["-n", "--", "true"]);
    } else {
        command.arg("-v");
    }
    let status = crate::execution::process::run_inherited(&mut command, "sudo authorization")?;
    if !status.success {
        return Err(WombatError::configuration(if noninteractive {
            "non-interactive bootstrap requires existing passwordless sudo authorization"
        } else {
            "sudo authorization failed"
        }));
    }
    Ok(())
}

pub(crate) fn mutating_status(
    program: &Path,
    args: &[&str],
    environment: &BTreeMap<String, String>,
    elevated: bool,
    noninteractive: bool,
) -> Result<ProcessOutcome> {
    let through_sudo = elevated && !effective_uid_is_root()?;
    let mut command = if through_sudo {
        let sudo = require_command("sudo", "elevated provider mutation")?;
        let mut command = Command::new(sudo);
        if noninteractive {
            command.arg("-n");
        }
        command.arg("--");
        if !environment.is_empty() {
            command.arg("env");
            for (name, value) in environment {
                command.arg(format!("{name}={value}"));
            }
        }
        command.arg(program);
        command
    } else {
        Command::new(program)
    };
    command.args(args);
    if !through_sudo {
        command.envs(environment);
    }
    crate::execution::process::run_inherited(&mut command, &program.display().to_string())
}

pub(crate) fn run_mutating(
    program: &Path,
    args: &[&str],
    environment: &BTreeMap<String, String>,
    elevated: bool,
    noninteractive: bool,
) -> Result<()> {
    let status = mutating_status(program, args, environment, elevated, noninteractive)?;
    if status.success {
        Ok(())
    } else {
        Err(WombatError::configuration(format!(
            "provider command `{}` failed with {}",
            program.display(),
            status.status
        )))
    }
}

pub(crate) fn observe_command_version(path: &Path) -> Result<String> {
    let output = run_bounded(path, &["--version"], &BTreeMap::new())?;
    if !output.success {
        return Err(WombatError::configuration(format!(
            "version probe `{}` failed: {}",
            path.display(),
            output_detail(&output)
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout.bytes);
    let stderr = String::from_utf8_lossy(&output.stderr.bytes);
    first_version(&stdout)
        .or_else(|| first_version(&stderr))
        .ok_or_else(|| {
            WombatError::configuration(format!(
                "could not parse a version from `{}`",
                path.display()
            ))
        })
}

pub(crate) fn first_version(text: &str) -> Option<String> {
    text.split_whitespace()
        .map(|part| {
            part.trim_matches(|character: char| {
                !character.is_ascii_alphanumeric() && character != '.'
            })
        })
        .find(|part| {
            part.chars()
                .next()
                .is_some_and(|character| character.is_ascii_digit())
                && part.contains('.')
        })
        .map(str::to_string)
}

pub(crate) fn version_at_least(observed: &str, minimum: &str) -> bool {
    let parts = |value: &str| {
        value
            .split(|character: char| !character.is_ascii_digit())
            .filter(|part| !part.is_empty())
            .take(4)
            .map(|part| part.parse::<u64>().unwrap_or(0))
            .collect::<Vec<_>>()
    };
    let mut observed = parts(observed);
    let mut minimum = parts(minimum);
    let length = observed.len().max(minimum.len());
    observed.resize(length, 0);
    minimum.resize(length, 0);
    observed >= minimum
}

pub(crate) fn which(command: &str) -> Option<PathBuf> {
    if command.contains(std::path::MAIN_SEPARATOR) {
        let path = PathBuf::from(command);
        return is_executable_file(&path).then_some(path);
    }
    env::split_paths(&env::var_os("PATH")?).find_map(|directory| {
        let path = directory.join(command);
        is_executable_file(&path).then_some(path)
    })
}

pub(crate) fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

pub(crate) fn run_bounded(
    path: &Path,
    args: &[&str],
    environment: &BTreeMap<String, String>,
) -> Result<ProcessOutcome> {
    let mut command = Command::new(path);
    command.args(args).envs(environment);
    let output = crate::execution::process::run(
        &mut command,
        &path.display().to_string(),
        None,
        OUTPUT_LIMIT,
        None,
        crate::execution::process::Forwarding::Retained,
    )?;
    if output.stdout.truncated || output.stderr.truncated {
        return Err(WombatError::configuration(format!(
            "process `{}` exceeded the {} byte observation limit",
            path.display(),
            OUTPUT_LIMIT
        )));
    }
    Ok(output)
}

pub(crate) fn output_detail(output: &ProcessOutcome) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr.bytes);
    let stdout = String::from_utf8_lossy(&output.stdout.bytes);
    stderr
        .trim()
        .lines()
        .next()
        .or_else(|| stdout.trim().lines().next())
        .unwrap_or("no diagnostic output")
        .to_string()
}
