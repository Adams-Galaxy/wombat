use std::path::{Component, Path};

use crate::manifest::{InferenceBasis, SourceAnchor, TargetAnchor, TargetOrigin, TargetPath};
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

pub(crate) fn infer_target(
    source_anchor: SourceAnchor,
    relative_path: &str,
    basis: InferenceBasis,
) -> Result<TargetPath> {
    validate_relative_path(relative_path, "artifact target path")?;
    let anchor = source_anchor.target_anchor();
    Ok(TargetPath {
        anchor,
        path: relative_path.to_string(),
        display: display_target(anchor, relative_path),
        origin: TargetOrigin::Inferred {
            basis,
            source_anchor,
        },
    })
}

pub(crate) fn parse_explicit_target(value: &str) -> Result<TargetPath> {
    let Some(relative) = value.strip_prefix("~/") else {
        return Err(WombatError::configuration(format!(
            "invalid target `{value}`; explicit targets must begin with `~/`"
        )));
    };
    validate_relative_path(relative, "target")?;

    let (anchor, path) = if let Some(config_path) = relative.strip_prefix(".config/") {
        if config_path.is_empty() {
            return Err(WombatError::configuration(format!(
                "invalid target `{value}`; a file target must not be an anchor root"
            )));
        }
        (TargetAnchor::Config, config_path)
    } else if relative == ".config" {
        return Err(WombatError::configuration(format!(
            "invalid target `{value}`; a file target must not be an anchor root"
        )));
    } else {
        (TargetAnchor::Home, relative)
    };

    Ok(TargetPath {
        anchor,
        path: path.to_string(),
        display: display_target(anchor, path),
        origin: TargetOrigin::Explicit {
            declared: value.to_string(),
        },
    })
}

pub(crate) fn prefixed_source(value: &str) -> Result<Option<(SourceAnchor, &str)>> {
    if value == ".config" || value.starts_with(".config/") {
        return Err(WombatError::configuration(format!(
            "unsupported source path `{value}`; use the `dot_config/` source anchor instead of `.config/`"
        )));
    }
    if let Some(relative) = value.strip_prefix("dot_config/") {
        validate_relative_path(relative, "artifact source")?;
        return Ok(Some((SourceAnchor::DotConfig, relative)));
    }
    if let Some(relative) = value.strip_prefix("home/") {
        validate_relative_path(relative, "artifact source")?;
        return Ok(Some((SourceAnchor::Home, relative)));
    }
    Ok(None)
}

pub(crate) fn display_target(anchor: TargetAnchor, path: &str) -> String {
    match anchor {
        TargetAnchor::Home => format!("~/{path}"),
        TargetAnchor::Config => format!("~/.config/{path}"),
    }
}

pub(crate) fn reject_legacy_config_tree(root: &Path) -> Result<()> {
    for path in [root.join(".config"), root.join("modules").join(".config")] {
        if path
            .try_exists()
            .map_err(|error| WombatError::io(&path, error))?
        {
            return Err(WombatError::configuration(format!(
                "unsupported source tree `{}`; use `dot_config/` to make source state explicit",
                path.strip_prefix(root).unwrap_or(&path).display()
            )));
        }
    }
    Ok(())
}

impl SourceAnchor {
    pub(crate) fn target_anchor(self) -> TargetAnchor {
        match self {
            Self::Home => TargetAnchor::Home,
            Self::DotConfig => TargetAnchor::Config,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::manifest::{InferenceBasis, SourceAnchor, TargetAnchor, TargetOrigin};

    use super::{infer_target, parse_explicit_target, prefixed_source, validate_relative_path};

    #[test]
    fn validates_portable_relative_paths() {
        assert!(validate_relative_path("nvim/init.lua", "source").is_ok());
        assert!(validate_relative_path("../init.lua", "source").is_err());
        assert!(validate_relative_path("nvim//init.lua", "source").is_err());
        assert!(validate_relative_path("nvim\\init.lua", "source").is_err());
        assert!(validate_relative_path("./init.lua", "source").is_err());
    }

    #[test]
    fn normalizes_explicit_targets_to_semantic_anchors() {
        let config = parse_explicit_target("~/.config/starship.toml").unwrap();
        assert_eq!(config.anchor, TargetAnchor::Config);
        assert_eq!(config.path, "starship.toml");
        assert_eq!(config.display, "~/.config/starship.toml");
        assert_eq!(
            config.origin,
            TargetOrigin::Explicit {
                declared: "~/.config/starship.toml".to_string()
            }
        );

        let home = parse_explicit_target("~/.zshrc").unwrap();
        assert_eq!(home.anchor, TargetAnchor::Home);
        assert_eq!(home.path, ".zshrc");
        assert!(parse_explicit_target(".config/starship.toml").is_err());
        assert!(parse_explicit_target("~/.config").is_err());
    }

    #[test]
    fn infers_only_recognized_source_prefixes() {
        assert_eq!(
            prefixed_source("dot_config/starship.toml").unwrap(),
            Some((SourceAnchor::DotConfig, "starship.toml"))
        );
        assert_eq!(
            prefixed_source("home/.zshrc").unwrap(),
            Some((SourceAnchor::Home, ".zshrc"))
        );
        assert!(prefixed_source(".config/starship.toml").is_err());
        assert_eq!(prefixed_source("other/file").unwrap(), None);

        let target = infer_target(
            SourceAnchor::DotConfig,
            "starship.toml",
            InferenceBasis::SourcePrefix,
        )
        .unwrap();
        assert_eq!(target.anchor, TargetAnchor::Config);
    }
}
