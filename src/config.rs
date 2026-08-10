use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::{Result, WombatError};

const CONFIG_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UserConfig {
    format_version: u32,
    repository: String,
}

#[doc(hidden)]
pub fn resolve_source(explicit: Option<&Path>) -> Result<PathBuf> {
    let current = env::current_dir().map_err(|error| WombatError::io(".", error))?;
    let home = env::var_os("HOME").map(PathBuf::from);
    let xdg = env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    resolve_source_with(explicit, &current, home.as_deref(), xdg.as_deref())
}

#[doc(hidden)]
pub fn resolve_home() -> Result<PathBuf> {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| WombatError::configuration("HOME is not set"))?;
    fs::canonicalize(&home).map_err(|error| WombatError::io(&home, error))
}

pub(crate) fn resolve_source_with(
    explicit: Option<&Path>,
    current: &Path,
    home: Option<&Path>,
    xdg_config_home: Option<&Path>,
) -> Result<PathBuf> {
    let selected = if let Some(explicit) = explicit {
        if explicit.is_absolute() {
            explicit.to_path_buf()
        } else {
            current.join(explicit)
        }
    } else {
        let home = home.ok_or_else(|| {
            WombatError::configuration(
                "HOME is not set; pass `--source` explicitly or configure a home directory",
            )
        })?;
        let config_root = match xdg_config_home {
            Some(path) if path.is_absolute() => path.to_path_buf(),
            Some(path) => {
                return Err(WombatError::configuration(format!(
                    "XDG_CONFIG_HOME must be absolute, got `{}`",
                    path.display()
                )));
            }
            None => home.join(".config"),
        };
        let config_path = config_root.join("wombat/config.toml");
        match fs::read_to_string(&config_path) {
            Ok(contents) => repository_from_config(&config_path, &contents, home)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                home.join(".local/share/wombat")
            }
            Err(error) => return Err(WombatError::io(&config_path, error)),
        }
    };

    fs::canonicalize(&selected).map_err(|error| WombatError::io(&selected, error))
}

fn repository_from_config(path: &Path, contents: &str, home: &Path) -> Result<PathBuf> {
    let config: UserConfig = toml::from_str(contents).map_err(|error| {
        WombatError::configuration(format!(
            "failed to parse Wombat config `{}`: {error}",
            path.display()
        ))
    })?;
    if config.format_version != CONFIG_FORMAT_VERSION {
        return Err(WombatError::configuration(format!(
            "unsupported Wombat config format version {} in `{}`; expected {CONFIG_FORMAT_VERSION}",
            config.format_version,
            path.display()
        )));
    }
    if config.repository == "~" {
        return Ok(home.to_path_buf());
    }
    if let Some(relative) = config.repository.strip_prefix("~/") {
        if relative.is_empty() {
            return Ok(home.to_path_buf());
        }
        return Ok(home.join(relative));
    }
    let repository = PathBuf::from(&config.repository);
    if !repository.is_absolute() {
        return Err(WombatError::configuration(format!(
            "repository in `{}` must be absolute or begin with `~/`",
            path.display()
        )));
    }
    Ok(repository)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use super::resolve_source_with;

    #[test]
    fn explicit_source_bypasses_host_configuration() {
        let temporary = tempdir().unwrap();
        let source = temporary.path().join("source");
        fs::create_dir(&source).unwrap();
        assert_eq!(
            resolve_source_with(Some(&source), temporary.path(), None, None).unwrap(),
            source.canonicalize().unwrap()
        );
    }

    #[test]
    fn reads_xdg_config_and_expands_home() {
        let temporary = tempdir().unwrap();
        let home = temporary.path().join("home");
        let xdg = temporary.path().join("xdg");
        let source = home.join("dotfiles");
        fs::create_dir_all(xdg.join("wombat")).unwrap();
        fs::create_dir_all(&source).unwrap();
        fs::write(
            xdg.join("wombat/config.toml"),
            "format_version = 1\nrepository = \"~/dotfiles\"\n",
        )
        .unwrap();
        assert_eq!(
            resolve_source_with(None, temporary.path(), Some(&home), Some(&xdg)).unwrap(),
            source.canonicalize().unwrap()
        );
    }

    #[test]
    fn rejects_relative_xdg_and_config_repository() {
        let temporary = tempdir().unwrap();
        let home = temporary.path().join("home");
        fs::create_dir_all(&home).unwrap();
        assert!(
            resolve_source_with(
                None,
                temporary.path(),
                Some(&home),
                Some(Path::new("relative"))
            )
            .unwrap_err()
            .to_string()
            .contains("must be absolute")
        );

        let config = home.join(".config/wombat");
        fs::create_dir_all(&config).unwrap();
        fs::write(
            config.join("config.toml"),
            "format_version = 1\nrepository = \"relative\"\n",
        )
        .unwrap();
        assert!(
            resolve_source_with(None, temporary.path(), Some(&home), None)
                .unwrap_err()
                .to_string()
                .contains("must be absolute")
        );
    }
}
