//! Human diff rendering and target compatibility policy.

use super::apply::product_path;
use super::*;

pub(super) fn render_diff(
    opened: &OpenedBuild,
    plan: &ReconciliationPlan,
    all_patches: bool,
) -> Result<String> {
    let mut output = String::new();
    let mut counts = BTreeMap::<&'static str, usize>::new();
    for item in &plan.items {
        if item.action == ReconciliationAction::Unchanged {
            continue;
        }
        *counts.entry(action_word(item.action)).or_default() += 1;
        let include_patch = all_patches
            || matches!(
                item.action,
                ReconciliationAction::Update | ReconciliationAction::Conflict
            );
        render_item(&mut output, opened, &plan.target_root, item, include_patch)?;
    }
    if output.is_empty() {
        output.push_str("No differences.\n");
    } else {
        use std::fmt::Write as _;
        let total = counts.values().sum::<usize>();
        let summary = counts
            .into_iter()
            .map(|(action, count)| format!("{count} {action}"))
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(&mut output, "{total} changes: {summary}")
            .expect("writing to a string cannot fail");
    }
    Ok(output)
}

pub(super) fn render_item(
    output: &mut String,
    opened: &OpenedBuild,
    target_root: &Path,
    item: &crate::reconcile::ReconciliationItem,
    include_patch: bool,
) -> Result<()> {
    use std::fmt::Write as _;
    writeln!(output, "{:?} {}", item.action, item.target).expect("writing to a string cannot fail");
    if let Some(artifact) = item.desired.as_ref().or(item.previous.as_ref()) {
        let producer = match artifact.production {
            Production::Static => "static",
            Production::Template { .. } => "template",
            Production::GeneratedLua { .. } => "generated Lua",
            Production::Task { .. } => "task",
        };
        writeln!(
            output,
            "  owner: {}\n  source: {}\n  production: {producer}",
            artifact.owner, artifact.source
        )
        .expect("writing to a string cannot fail");
    }
    if let Some(reason) = &item.reason {
        writeln!(output, "  conflict: {reason}").expect("writing to a string cannot fail");
    }
    if include_patch {
        append_content_diff(output, opened, target_root, item)?;
    }
    Ok(())
}

const fn action_word(action: ReconciliationAction) -> &'static str {
    match action {
        ReconciliationAction::Unchanged => "unchanged",
        ReconciliationAction::Create => "create",
        ReconciliationAction::Adopt => "adopt",
        ReconciliationAction::AdvanceState => "state-only",
        ReconciliationAction::Update => "update",
        ReconciliationAction::Remove => "remove",
        ReconciliationAction::Forget => "forget",
        ReconciliationAction::Conflict => "conflict",
    }
}

pub(super) fn validate_target_compatibility(
    manifest: &crate::manifest::Manifest,
    host: &HostContext,
    target_root_explicit: bool,
) -> Result<Vec<String>> {
    let target_os = manifest.target.platform.os.name;
    let host_os = host.platform.os.name;
    if !target_root_explicit && target_os != host_os {
        return Err(WombatError::configuration(format!(
            "build target OS `{}` ({:?}) does not match host OS `{}`; refusing implicit live-root deployment before mutation; pass --target-root deliberately for an alternate root",
            target_os.as_str(),
            manifest.target.origin,
            host_os.as_str()
        )));
    }
    let mut warnings = Vec::new();
    if manifest.target.platform.arch != host.platform.arch {
        warnings.push(format!(
            "build target architecture `{}` differs from host architecture `{}`",
            manifest.target.platform.arch.as_str(),
            host.platform.arch.as_str()
        ));
    }
    Ok(warnings)
}

fn append_content_diff(
    output: &mut String,
    opened: &OpenedBuild,
    target_root: &Path,
    item: &crate::reconcile::ReconciliationItem,
) -> Result<()> {
    let old = if matches!(item.actual, ActualArtifact::File { .. }) {
        let bytes = fs::read(&item.path).map_err(|error| WombatError::io(&item.path, error))?;
        let after = inspect_actual(target_root, &item.path)?;
        if after != item.actual {
            return Err(WombatError::configuration(format!(
                "target `{}` changed while its diff was rendered",
                item.target
            )));
        }
        if let ActualArtifact::File { content, .. } = &item.actual
            && (u64::try_from(bytes.len()).ok() != Some(content.size)
                || digest_string(Sha256::digest(&bytes)) != content.digest)
        {
            return Err(WombatError::configuration(format!(
                "target `{}` changed while its diff was rendered",
                item.target
            )));
        }
        Some(bytes)
    } else {
        None
    };
    let new = item
        .desired
        .as_ref()
        .map(|artifact| {
            let path = product_path(opened, artifact);
            fs::read(&path).map_err(|error| WombatError::io(path, error))
        })
        .transpose()?;
    let old_bytes = old.as_deref().unwrap_or_default();
    let new_bytes = new.as_deref().unwrap_or_default();
    let text = !old_bytes.contains(&0)
        && !new_bytes.contains(&0)
        && std::str::from_utf8(old_bytes).is_ok()
        && std::str::from_utf8(new_bytes).is_ok();
    if text {
        let old_text = std::str::from_utf8(old_bytes).expect("text was validated");
        let new_text = std::str::from_utf8(new_bytes).expect("text was validated");
        let diff = similar::TextDiff::from_lines(old_text, new_text);
        let unified = diff
            .unified_diff()
            .context_radius(3)
            .header(&format!("a/{}", item.target), &format!("b/{}", item.target))
            .to_string();
        if !unified.is_empty() {
            output.push_str(&unified);
            if !unified.ends_with('\n') {
                output.push('\n');
            }
        }
    } else {
        use std::fmt::Write as _;
        let (old_digest, old_size, old_mode) = match &item.actual {
            ActualArtifact::File { content, mode } => {
                (content.digest.as_str(), content.size, format!("{mode:04o}"))
            }
            _ => ("absent", 0, "----".to_string()),
        };
        let (new_digest, new_size, new_mode) = item.desired.as_ref().map_or_else(
            || ("absent", 0, "----".to_string()),
            |artifact| {
                (
                    artifact.content.digest.as_str(),
                    artifact.content.size,
                    format!("{:04o}", crate::reconcile::expected_mode(artifact)),
                )
            },
        );
        writeln!(
            output,
            "  binary: {old_digest} {old_size} bytes mode {old_mode} -> {new_digest} {new_size} bytes mode {new_mode}"
        )
        .expect("writing to a string cannot fail");
    }
    Ok(())
}

pub(super) fn require_deployment_platform() -> Result<()> {
    if deployment_platform_supported(std::env::consts::OS) {
        Ok(())
    } else {
        Err(WombatError::configuration(
            "target deployment is currently supported only on macOS and Linux",
        ))
    }
}

fn deployment_platform_supported(os: &str) -> bool {
    matches!(os, "macos" | "linux")
}

#[cfg(test)]
mod tests {
    use super::deployment_platform_supported;

    #[test]
    fn deployment_platform_gate_accepts_only_macos_and_linux() {
        assert!(deployment_platform_supported("macos"));
        assert!(deployment_platform_supported("linux"));
        assert!(!deployment_platform_supported("windows"));
        assert!(!deployment_platform_supported("freebsd"));
    }
}
