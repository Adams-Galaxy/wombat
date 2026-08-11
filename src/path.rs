use std::path::{Component, Path};

use crate::manifest::{
    EvaluatedTargetOrigin, EvaluatedTargetRoot, InferenceBasis, SourceAnchor, TargetAnchor,
    TargetOrigin, TargetPath,
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
        validate_relative_path(value, "static artifact source")
    }
}

pub(crate) fn infer_target(
    source_anchor: SourceAnchor,
    relative_path: &str,
    basis: InferenceBasis,
) -> Result<TargetPath> {
    validate_relative_path(relative_path, "artifact target path")?;
    let root = infer_target_root(source_anchor, "", basis)?;
    expand_target_root(&root, relative_path)
}

pub(crate) fn infer_target_root(
    source_anchor: SourceAnchor,
    relative_path: &str,
    basis: InferenceBasis,
) -> Result<EvaluatedTargetRoot> {
    if !relative_path.is_empty() {
        validate_relative_path(relative_path, "directory target path")?;
    }
    let (anchor, prefix) = source_anchor.target_root();
    let path = join_relative(prefix, relative_path);
    Ok(EvaluatedTargetRoot {
        anchor,
        path,
        origin: EvaluatedTargetOrigin::Inferred {
            basis,
            source_anchor,
        },
    })
}

pub(crate) fn parse_explicit_target(value: &str) -> Result<TargetPath> {
    let root = parse_explicit_target_root(value)?;
    if root.path.is_empty() {
        return Err(WombatError::configuration(format!(
            "invalid target `{value}`; a file target must not be an anchor root"
        )));
    }
    let (anchor, path) = canonical_target(root.anchor, &root.path)?;
    Ok(TargetPath {
        anchor,
        display: display_target(anchor, &path),
        path,
        origin: TargetOrigin::Explicit {
            declared: value.to_string(),
        },
    })
}

pub(crate) fn parse_explicit_target_root(value: &str) -> Result<EvaluatedTargetRoot> {
    let Some(relative) = value.strip_prefix("~/") else {
        return Err(WombatError::configuration(format!(
            "invalid target `{value}`; explicit targets must begin with `~/`"
        )));
    };
    if !relative.is_empty() {
        validate_relative_path(relative, "target")?;
    }

    let (anchor, path) = if relative == ".config" {
        (TargetAnchor::Config, String::new())
    } else if let Some(config_path) = relative.strip_prefix(".config/") {
        (TargetAnchor::Config, config_path.to_string())
    } else {
        (TargetAnchor::Home, relative.to_string())
    };
    Ok(EvaluatedTargetRoot {
        anchor,
        path,
        origin: EvaluatedTargetOrigin::Explicit {
            declared: value.to_string(),
        },
    })
}

pub(crate) fn expand_target_root(root: &EvaluatedTargetRoot, relative: &str) -> Result<TargetPath> {
    validate_relative_path(relative, "expanded target path")?;
    let joined = join_relative(&root.path, relative);
    let (anchor, path) = canonical_target(root.anchor, &joined)?;
    let origin = match &root.origin {
        EvaluatedTargetOrigin::Explicit { declared } => TargetOrigin::DirectoryExplicit {
            declared: declared.clone(),
            relative: relative.to_string(),
        },
        EvaluatedTargetOrigin::Inferred {
            basis,
            source_anchor,
        } => TargetOrigin::Inferred {
            basis: *basis,
            source_anchor: *source_anchor,
        },
    };
    Ok(TargetPath {
        anchor,
        display: display_target(anchor, &path),
        path,
        origin,
    })
}

pub(crate) fn prefixed_source(value: &str) -> Result<Option<(SourceAnchor, &str)>> {
    for (legacy, canonical) in [(".config", "dot_config"), (".local", "dot_local")] {
        if value == legacy || value.starts_with(&format!("{legacy}/")) {
            return Err(WombatError::configuration(format!(
                "unsupported source path `{value}`; use the `{canonical}/` source anchor instead of `{legacy}/`"
            )));
        }
    }
    for (legacy, canonical) in [("home/.config", "dot_config"), ("home/.local", "dot_local")] {
        if value == legacy || value.starts_with(&format!("{legacy}/")) {
            return Err(WombatError::configuration(format!(
                "unsupported source path `{value}`; use the `{canonical}/` source anchor instead"
            )));
        }
    }

    for anchor in SourceAnchor::ALL {
        let prefix = anchor.source_prefix();
        if value == prefix {
            return Ok(Some((anchor, "")));
        }
        if let Some(relative) = value.strip_prefix(&format!("{prefix}/")) {
            validate_relative_path(relative, "artifact source")?;
            return Ok(Some((anchor, relative)));
        }
    }
    Ok(None)
}

pub(crate) fn display_target(anchor: TargetAnchor, path: &str) -> String {
    match anchor {
        TargetAnchor::Home => format!("~/{path}"),
        TargetAnchor::Config => format!("~/.config/{path}"),
    }
}

pub(crate) fn reject_noncanonical_artifact_trees(root: &Path) -> Result<()> {
    let candidates = [
        (root.join(".config"), "dot_config"),
        (root.join(".local"), "dot_local"),
        (root.join("home/.config"), "dot_config"),
        (root.join("home/.local"), "dot_local"),
        (root.join("modules/.config"), "modules/dot_config"),
        (root.join("modules/.local"), "modules/dot_local"),
    ];
    for (path, canonical) in candidates {
        if path
            .try_exists()
            .map_err(|error| WombatError::io(&path, error))?
        {
            return Err(WombatError::configuration(format!(
                "unsupported source tree `{}`; use `{canonical}/` to keep artifact source state canonical",
                path.strip_prefix(root).unwrap_or(&path).display()
            )));
        }
    }
    Ok(())
}

fn canonical_target(anchor: TargetAnchor, path: &str) -> Result<(TargetAnchor, String)> {
    if path.is_empty() {
        return Err(WombatError::configuration(
            "an expanded file target must not be an anchor root",
        ));
    }
    if anchor == TargetAnchor::Home {
        if path == ".config" {
            return Err(WombatError::configuration(
                "an expanded file target must not be the configuration anchor root",
            ));
        }
        if let Some(relative) = path.strip_prefix(".config/") {
            return Ok((TargetAnchor::Config, relative.to_string()));
        }
    }
    Ok((anchor, path.to_string()))
}

fn join_relative(left: &str, right: &str) -> String {
    match (left.is_empty(), right.is_empty()) {
        (true, _) => right.to_string(),
        (_, true) => left.to_string(),
        (false, false) => format!("{left}/{right}"),
    }
}

impl SourceAnchor {
    pub(crate) const ALL: [Self; 3] = [Self::Home, Self::DotConfig, Self::DotLocal];

    pub(crate) fn source_prefix(self) -> &'static str {
        match self {
            Self::Home => "home",
            Self::DotConfig => "dot_config",
            Self::DotLocal => "dot_local",
        }
    }

    pub(crate) fn target_root(self) -> (TargetAnchor, &'static str) {
        match self {
            Self::Home => (TargetAnchor::Home, ""),
            Self::DotConfig => (TargetAnchor::Config, ""),
            Self::DotLocal => (TargetAnchor::Home, ".local"),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::manifest::{InferenceBasis, SourceAnchor, TargetAnchor, TargetOrigin};

    use super::{
        expand_target_root, infer_target, parse_explicit_target, parse_explicit_target_root,
        prefixed_source, validate_relative_path,
    };

    #[test]
    fn validates_portable_relative_paths() {
        assert!(validate_relative_path("nvim/init.lua", "source").is_ok());
        assert!(validate_relative_path("../init.lua", "source").is_err());
        assert!(validate_relative_path("nvim//init.lua", "source").is_err());
        assert!(validate_relative_path("nvim\\init.lua", "source").is_err());
        assert!(validate_relative_path("./init.lua", "source").is_err());
    }

    #[test]
    fn normalizes_explicit_file_and_directory_targets() {
        let config = parse_explicit_target("~/.config/starship.toml").unwrap();
        assert_eq!(config.anchor, TargetAnchor::Config);
        assert_eq!(config.path, "starship.toml");
        assert_eq!(
            config.origin,
            TargetOrigin::Explicit {
                declared: "~/.config/starship.toml".to_string()
            }
        );
        assert!(parse_explicit_target("~/.config").is_err());
        assert!(parse_explicit_target("~/").is_err());

        let root = parse_explicit_target_root("~/").unwrap();
        let expanded = expand_target_root(&root, ".config/nvim/init.lua").unwrap();
        assert_eq!(expanded.anchor, TargetAnchor::Config);
        assert_eq!(expanded.path, "nvim/init.lua");
    }

    #[test]
    fn recognizes_canonical_source_prefixes() {
        assert_eq!(
            prefixed_source("dot_config").unwrap(),
            Some((SourceAnchor::DotConfig, ""))
        );
        assert_eq!(
            prefixed_source("dot_local/bin/tool").unwrap(),
            Some((SourceAnchor::DotLocal, "bin/tool"))
        );
        assert_eq!(
            prefixed_source("home/.zshrc").unwrap(),
            Some((SourceAnchor::Home, ".zshrc"))
        );
        assert!(prefixed_source("home/.config/nvim").is_err());
        assert!(prefixed_source(".local/bin").is_err());
        assert_eq!(prefixed_source("other/file").unwrap(), None);

        let target = infer_target(
            SourceAnchor::DotLocal,
            "bin/tool",
            InferenceBasis::SourcePrefix,
        )
        .unwrap();
        assert_eq!(target.anchor, TargetAnchor::Home);
        assert_eq!(target.path, ".local/bin/tool");
    }
}
