//! Target path validation and the rules for turning a declaration into a
//! deployment path.
//!
//! Everything a repository can name eventually passes through here. Root-relative
//! paths stay inside the target root; explicit POSIX absolute paths retain their
//! literal external endpoint. Neither form consults the deploying machine's
//! environment.
//!
//! Traversal, Windows separators, and shell expansion are refused rather than
//! normalised away, because silently reinterpreting a path the user wrote is how
//! a deployment ends up somewhere they did not explicitly choose.
use std::path::{Component, Path};

use crate::model::manifest::{
    EvaluatedTargetOrigin, EvaluatedTargetRoot, TargetOrigin, TargetPath, TargetScope,
};
use crate::{Result, WombatError};

pub(crate) fn validate_relative_path(value: &str, subject: &str) -> Result<()> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || value.contains('\\')
        || value
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(WombatError::configuration(format!(
            "invalid {subject} `{value}`; expected a UTF-8 relative path without traversal, empty components, or backslashes"
        )));
    }
    Ok(())
}

pub(crate) fn validate_declared_source(value: &str) -> Result<()> {
    if value == "." {
        Ok(())
    } else {
        validate_relative_path(value, "artifact source")
    }
}

pub(crate) fn infer_target(relative_path: &str, source: impl Into<String>) -> Result<TargetPath> {
    validate_relative_path(relative_path, "artifact target path")?;
    Ok(TargetPath {
        path: relative_path.to_string(),
        scope: TargetScope::DeploymentRoot,
        origin: TargetOrigin::Inferred {
            source: source.into(),
        },
    })
}

pub(crate) fn infer_target_root(
    relative_path: &str,
    source: impl Into<String>,
) -> Result<EvaluatedTargetRoot> {
    if !relative_path.is_empty() {
        validate_relative_path(relative_path, "directory target path")?;
    }
    Ok(EvaluatedTargetRoot {
        path: relative_path.to_string(),
        scope: TargetScope::DeploymentRoot,
        origin: EvaluatedTargetOrigin::Inferred {
            source: source.into(),
        },
    })
}

pub(crate) fn parse_explicit_target(value: &str) -> Result<TargetPath> {
    if value == "~" || value.starts_with("~/") || value.contains('%') {
        return Err(WombatError::configuration(
            "target paths must not use `~` or Windows `%VARIABLE%` expansion",
        ));
    }
    let scope = if Path::new(value).is_absolute() {
        validate_absolute_target(value)?;
        TargetScope::Absolute
    } else {
        validate_relative_path(value, "target")?;
        TargetScope::DeploymentRoot
    };
    Ok(TargetPath {
        path: value.to_string(),
        scope,
        origin: TargetOrigin::Explicit {
            declared: value.to_string(),
        },
    })
}

pub(crate) fn parse_explicit_target_root(value: &str) -> Result<EvaluatedTargetRoot> {
    if value == "~" || value.starts_with("~/") {
        return Err(WombatError::configuration(
            "target roots are deployment-root-relative and must not use `~`",
        ));
    }
    if !value.is_empty() {
        validate_relative_path(value, "target root")?;
    }
    Ok(EvaluatedTargetRoot {
        path: value.to_string(),
        scope: TargetScope::DeploymentRoot,
        origin: EvaluatedTargetOrigin::Explicit {
            declared: value.to_string(),
        },
    })
}

/// Directory and glob installs may explicitly target a second filesystem root.
/// Module bases deliberately keep the narrower deployment-root-relative rule.
pub(crate) fn parse_install_target_root(value: &str) -> Result<EvaluatedTargetRoot> {
    if value == "~" || value.starts_with("~/") || value.contains('%') {
        return Err(WombatError::configuration(
            "target roots must not use `~` or Windows `%VARIABLE%` expansion",
        ));
    }
    let scope = if Path::new(value).is_absolute() {
        validate_absolute_target(value)?;
        TargetScope::Absolute
    } else {
        if !value.is_empty() {
            validate_relative_path(value, "target root")?;
        }
        TargetScope::DeploymentRoot
    };
    Ok(EvaluatedTargetRoot {
        path: value.to_string(),
        scope,
        origin: EvaluatedTargetOrigin::Explicit {
            declared: value.to_string(),
        },
    })
}

pub(crate) fn expand_target_root(root: &EvaluatedTargetRoot, relative: &str) -> Result<TargetPath> {
    validate_relative_path(relative, "expanded target path")?;
    let path = match root.scope {
        TargetScope::DeploymentRoot => {
            let path = join_relative(&root.path, relative);
            validate_relative_path(&path, "expanded target path")?;
            path
        }
        TargetScope::Absolute => {
            let path = Path::new(&root.path).join(relative);
            let path = path.to_str().ok_or_else(|| {
                WombatError::configuration("absolute target path must be valid UTF-8")
            })?;
            validate_absolute_target(path)?;
            path.to_string()
        }
    };
    let origin = match &root.origin {
        EvaluatedTargetOrigin::Explicit { declared } => TargetOrigin::DirectoryExplicit {
            declared: declared.clone(),
            relative: relative.to_string(),
        },
        EvaluatedTargetOrigin::Inferred { source } => TargetOrigin::Inferred {
            source: source.clone(),
        },
    };
    Ok(TargetPath {
        path,
        scope: root.scope,
        origin,
    })
}

pub(crate) fn display_target(path: &str) -> String {
    path.to_string()
}

pub(crate) fn validate_absolute_target(value: &str) -> Result<()> {
    let path = Path::new(value);
    if !path.is_absolute()
        || value.contains('\\')
        || value
            .split('/')
            .any(|component| component == "." || component == "..")
        || path
            .components()
            .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
    {
        return Err(WombatError::configuration(format!(
            "invalid absolute target `{value}`; expected a UTF-8 POSIX absolute path without traversal or backslashes"
        )));
    }
    Ok(())
}

pub(crate) fn reject_legacy_artifact_trees(root: &Path) -> Result<()> {
    for name in ["home", "dot_config", "dot_local"] {
        let path = root.join(name);
        if path
            .try_exists()
            .map_err(|error| WombatError::io(&path, error))?
        {
            return Err(WombatError::configuration(format!(
                "legacy source tree `{name}/` is unsupported; move it beneath `src/` and update modules to use `w.module.from()`"
            )));
        }
    }
    Ok(())
}

pub(crate) fn join_relative(left: &str, right: &str) -> String {
    match (left.is_empty(), right.is_empty()) {
        (true, _) => right.to_string(),
        (_, true) => left.to_string(),
        (false, false) => format!("{left}/{right}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        expand_target_root, infer_target, parse_explicit_target, parse_explicit_target_root,
    };

    #[test]
    fn generic_targets_are_root_relative() {
        let inferred = infer_target(".config/app.toml", "src/dot_config/app.toml").unwrap();
        assert_eq!(inferred.path, ".config/app.toml");

        let explicit = parse_explicit_target(".local/bin/tool").unwrap();
        assert_eq!(explicit.path, ".local/bin/tool");
        assert!(parse_explicit_target("~/.config/app").is_err());
        assert!(parse_explicit_target("/etc/app").is_ok());

        let root = parse_explicit_target_root(".config/nvim").unwrap();
        let expanded = expand_target_root(&root, "init.lua").unwrap();
        assert_eq!(expanded.path, ".config/nvim/init.lua");
    }

    #[test]
    fn every_traversal_shape_is_rejected_at_target_boundaries() {
        for path in [
            "../escape",
            "safe/../escape",
            "./local",
            "safe//file",
            "safe\\file",
        ] {
            assert!(parse_explicit_target(path).is_err(), "accepted `{path}`");
        }
        for path in ["name", ".config/name", "nested/deep/name", "@literal"] {
            assert!(parse_explicit_target(path).is_ok(), "rejected `{path}`");
        }
    }
}
