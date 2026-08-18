//! Git-backed package binding and checkout verification.

use super::*;

pub(crate) fn check_git(binding: &ProviderBinding) -> Result<CheckItem> {
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
    // A satisfied check stays local: reconcile already fetched the pinned ref.
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

pub(crate) fn preflight_git(requirement: &Requirement) -> Result<()> {
    let (repository, to, _reference) = git_identity(&requirement.binding)?;
    let git = require_command("git", "Git preflight")?;
    if !confirm_or_absent_git_checkout(&git, to, repository)? {
        let probe = run_bounded(
            &git,
            &["ls-remote", "--exit-code", repository],
            &BTreeMap::new(),
        )?;
        if !probe.success {
            return Err(WombatError::configuration(format!(
                "Git preflight failed for `{repository}`: {}",
                output_detail(&probe)
            )));
        }
    }
    Ok(())
}

pub(crate) fn reconcile_git(requirement: &Requirement, noninteractive: bool) -> Result<()> {
    let (repository, to, reference) = git_identity(&requirement.binding)?;
    let git = require_command("git", "Git bootstrap")?;
    if !confirm_or_absent_git_checkout(&git, to, repository)? {
        if let Some(parent) = Path::new(to).parent() {
            fs::create_dir_all(parent).map_err(|error| WombatError::io(parent, error))?;
        }
        run_mutating(
            &git,
            &["clone", "--", repository, to],
            &BTreeMap::new(),
            false,
            noninteractive,
        )?;
    }
    if let Some(reference) = reference {
        run_mutating(
            &git,
            &["-C", to, "fetch", "--tags", "--", "origin"],
            &BTreeMap::new(),
            false,
            noninteractive,
        )?;
        run_mutating(
            &git,
            &["-C", to, "checkout", reference, "--"],
            &BTreeMap::new(),
            false,
            noninteractive,
        )?;
    }
    Ok(())
}

pub(crate) fn git_identity(binding: &ProviderBinding) -> Result<(&str, &str, Option<&str>)> {
    let FrozenValue::Map(data) = &binding.data else {
        return Err(WombatError::configuration("Git binding data must be a map"));
    };
    let repository = match data.get("repository") {
        Some(FrozenValue::String(value)) => value.as_str(),
        _ => return Err(WombatError::configuration("Git binding lacks repository")),
    };
    let to = match data.get("to") {
        Some(FrozenValue::String(value)) => value.as_str(),
        _ => return Err(WombatError::configuration("Git binding lacks destination")),
    };
    let reference = match data.get("ref") {
        None => None,
        Some(FrozenValue::String(value)) => Some(value.as_str()),
        _ => return Err(WombatError::configuration("Git binding has an invalid ref")),
    };
    Ok((repository, to, reference))
}

/// Reports whether `to` is already a checkout of `repository`. An existing
/// directory that isn't is left untouched rather than reused or replaced —
/// only an absent destination is safe to clone into.
pub(crate) fn confirm_or_absent_git_checkout(
    git: &Path,
    to: &str,
    repository: &str,
) -> Result<bool> {
    if !Path::new(to).join(".git").is_dir() {
        return Ok(false);
    }
    let remote = run_bounded(
        git,
        &["-C", to, "remote", "get-url", "origin"],
        &BTreeMap::new(),
    )?;
    let observed = String::from_utf8_lossy(&remote.stdout.bytes)
        .trim()
        .to_string();
    if !remote.success || observed != repository {
        return Err(WombatError::configuration(format!(
            "{to} already exists and is not a checkout of `{repository}`; resolve it manually"
        )));
    }
    Ok(true)
}
