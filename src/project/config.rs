use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::{Result, WombatError};

const CONFIG_FORMAT_VERSION: u32 = 2;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UserConfig {
    format_version: u32,
    repository: String,
    #[serde(default)]
    runners: BTreeMap<String, InterpreterConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InterpreterConfig {
    command: String,
    #[serde(default)]
    args: Vec<String>,
}

#[doc(hidden)]
pub fn resolve_source(explicit: Option<&Path>) -> Result<PathBuf> {
    let current = env::current_dir().map_err(|error| WombatError::io(".", error))?;
    let home = env::var_os("HOME").map(PathBuf::from);
    let xdg = env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    resolve_source_with(explicit, &current, home.as_deref(), xdg.as_deref())
}

#[doc(hidden)]
pub fn resolve_source_candidate(explicit: Option<&Path>) -> Result<PathBuf> {
    let current = env::current_dir().map_err(|error| WombatError::io(".", error))?;
    let home = env::var_os("HOME").map(PathBuf::from);
    let xdg = env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    select_source_with(explicit, &current, home.as_deref(), xdg.as_deref())
}

#[doc(hidden)]
pub fn resolve_home() -> Result<PathBuf> {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| WombatError::configuration("HOME is not set"))?;
    fs::canonicalize(&home).map_err(|error| WombatError::io(&home, error))
}

#[doc(hidden)]
pub fn resolve_runners() -> Result<BTreeMap<String, crate::model::manifest::TaskRunner>> {
    let home = env::var_os("HOME").map(PathBuf::from);
    let config_root = match env::var_os("XDG_CONFIG_HOME").map(PathBuf::from) {
        Some(path) if path.is_absolute() => path,
        Some(path) => {
            return Err(WombatError::configuration(format!(
                "XDG_CONFIG_HOME must be absolute, got `{}`",
                path.display()
            )));
        }
        None => match &home {
            Some(home) => home.join(".config"),
            None => return Ok(BTreeMap::new()),
        },
    };
    let path = config_root.join("wombat/config.toml");
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(WombatError::io(&path, error)),
    };
    configured_task_interpreters(parse_config(&path, &contents)?, &path, home.as_deref())
}

#[cfg(test)]
fn task_interpreters_from_config(
    path: &Path,
    contents: &str,
    home: &Path,
) -> Result<BTreeMap<String, crate::model::manifest::TaskRunner>> {
    configured_task_interpreters(parse_config(path, contents)?, path, Some(home))
}

fn configured_task_interpreters(
    config: UserConfig,
    path: &Path,
    home: Option<&Path>,
) -> Result<BTreeMap<String, crate::model::manifest::TaskRunner>> {
    let mut interpreters = BTreeMap::new();
    for (name, configured) in config.runners {
        let family = match name.as_str() {
            "python" => crate::model::manifest::InterpreterFamily::Python,
            "shell" => crate::model::manifest::InterpreterFamily::PosixShell,
            "bash" => crate::model::manifest::InterpreterFamily::Bash,
            "lua" => crate::model::manifest::InterpreterFamily::Custom,
            _ => {
                return Err(WombatError::configuration(format!(
                    "unknown task interpreter `{name}` in `{}`; expected python, shell, bash, or lua",
                    path.display()
                )));
            }
        };
        if configured.command.is_empty() {
            return Err(WombatError::configuration(format!(
                "task interpreter `{name}` in `{}` requires a non-empty command",
                path.display()
            )));
        }
        let command = expand_interpreter_command(&configured.command, home, path)?;
        interpreters.insert(
            name,
            crate::model::manifest::TaskRunner::Interpreter {
                contract_version: 1,
                family,
                command,
                args: configured.args,
            },
        );
    }
    Ok(interpreters)
}

pub(crate) fn resolve_source_with(
    explicit: Option<&Path>,
    current: &Path,
    home: Option<&Path>,
    xdg_config_home: Option<&Path>,
) -> Result<PathBuf> {
    let selected = select_source_with(explicit, current, home, xdg_config_home)?;

    fs::canonicalize(&selected).map_err(|error| WombatError::io(&selected, error))
}

fn select_source_with(
    explicit: Option<&Path>,
    current: &Path,
    home: Option<&Path>,
    xdg_config_home: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(explicit) = explicit {
        if explicit.is_absolute() {
            Ok(explicit.to_path_buf())
        } else {
            Ok(current.join(explicit))
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
            Ok(contents) => repository_from_config(&config_path, &contents, home),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(home.join(".local/share/wombat"))
            }
            Err(error) => Err(WombatError::io(&config_path, error)),
        }
    }
}

fn repository_from_config(path: &Path, contents: &str, home: &Path) -> Result<PathBuf> {
    let config = parse_config(path, contents)?;
    repository_path(path, &config.repository, home)
}

fn parse_config(path: &Path, contents: &str) -> Result<UserConfig> {
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
    Ok(config)
}

/// Where the resolved source repository came from.
///
/// Wombat never infers the repository from the working directory, so telling a
/// user which of these applied is usually the answer to "why is it building
/// that?".
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceOrigin {
    /// An explicit `--source` argument.
    Explicit,
    /// The `repository` key in the user configuration file.
    Configured,
    /// The built-in default beneath the home directory.
    Default,
}

/// The resolved source repository, where that choice came from, and the user
/// configuration path it was read from.
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct SourceResolution {
    pub source: PathBuf,
    pub origin: SourceOrigin,
    pub config_path: PathBuf,
    pub config_exists: bool,
}

#[doc(hidden)]
pub fn describe_source(explicit: Option<&Path>) -> Result<SourceResolution> {
    let config_path = user_config_path()?;
    let config_exists = config_path.exists();
    let origin = if explicit.is_some() {
        SourceOrigin::Explicit
    } else if config_exists {
        SourceOrigin::Configured
    } else {
        SourceOrigin::Default
    };
    Ok(SourceResolution {
        source: resolve_source_candidate(explicit)?,
        origin,
        config_path,
        config_exists,
    })
}

#[doc(hidden)]
pub fn user_config_path() -> Result<PathBuf> {
    let config_root = match env::var_os("XDG_CONFIG_HOME").map(PathBuf::from) {
        Some(path) if path.is_absolute() => path,
        Some(path) => {
            return Err(WombatError::configuration(format!(
                "XDG_CONFIG_HOME must be absolute, got `{}`",
                path.display()
            )));
        }
        None => {
            let home = env::var_os("HOME").map(PathBuf::from).ok_or_else(|| {
                WombatError::configuration("HOME is not set; cannot locate Wombat configuration")
            })?;
            home.join(".config")
        }
    };
    Ok(config_root.join("wombat/config.toml"))
}

/// Records `repository` in the user configuration, creating the file when it is
/// absent.
///
/// Rewrites only the `repository` line so hand-written comments, `[runners]`
/// entries, and formatting survive.
#[doc(hidden)]
pub fn set_configured_source(source: &Path) -> Result<PathBuf> {
    if !source.is_absolute() {
        return Err(WombatError::configuration(format!(
            "source `{}` must be absolute",
            source.display()
        )));
    }
    let path = user_config_path()?;
    let existing = match fs::read_to_string(&path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(WombatError::io(&path, error)),
    };
    let value = source.to_string_lossy();
    let updated = match existing {
        None => format!("format_version = {CONFIG_FORMAT_VERSION}\nrepository = \"{value}\"\n"),
        Some(contents) => rewrite_repository(&contents, &value),
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| WombatError::io(parent, error))?;
    }
    crate::storage::atomic::write_bytes(&path, updated.as_bytes(), false)?;
    Ok(path)
}

fn rewrite_repository(contents: &str, value: &str) -> String {
    let replacement = format!("repository = \"{value}\"");
    let mut lines: Vec<String> = Vec::new();
    let mut replaced = false;
    let mut in_table = false;
    for line in contents.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            in_table = true;
        }
        if !in_table && !replaced && trimmed.starts_with("repository") {
            lines.push(replacement.clone());
            replaced = true;
            continue;
        }
        lines.push(line.to_string());
    }
    if !replaced {
        // Keep the key above any table header, where top-level keys must live.
        let insert_at = lines
            .iter()
            .position(|line| line.trim_start().starts_with('['))
            .unwrap_or(lines.len());
        lines.insert(insert_at, replacement);
    }
    let mut updated = lines.join("\n");
    updated.push('\n');
    updated
}

fn repository_path(path: &Path, value: &str, home: &Path) -> Result<PathBuf> {
    if value == "~" {
        return Ok(home.to_path_buf());
    }
    if let Some(relative) = value.strip_prefix("~/") {
        if relative.is_empty() {
            return Ok(home.to_path_buf());
        }
        return Ok(home.join(relative));
    }
    let repository = PathBuf::from(value);
    if !repository.is_absolute() {
        return Err(WombatError::configuration(format!(
            "repository in `{}` must be absolute or begin with `~/`",
            path.display()
        )));
    }
    Ok(repository)
}

fn expand_interpreter_command(value: &str, home: Option<&Path>, path: &Path) -> Result<String> {
    if let Some(relative) = value.strip_prefix("~/") {
        let home = home.ok_or_else(|| {
            WombatError::configuration(format!(
                "task interpreter command `{value}` in `{}` requires HOME for `~/` expansion",
                path.display()
            ))
        })?;
        return Ok(home.join(relative).to_string_lossy().into_owned());
    }
    let command = Path::new(value);
    if command.components().count() > 1 && !command.is_absolute() {
        return Err(WombatError::configuration(format!(
            "task interpreter command `{value}` in `{}` must be a bare command, absolute path, or begin with `~/`",
            path.display()
        )));
    }
    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use super::{resolve_source_with, rewrite_repository, task_interpreters_from_config};

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
            "format_version = 2\nrepository = \"~/dotfiles\"\n",
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
            "format_version = 2\nrepository = \"relative\"\n",
        )
        .unwrap();
        assert!(
            resolve_source_with(None, temporary.path(), Some(&home), None)
                .unwrap_err()
                .to_string()
                .contains("must be absolute")
        );
    }

    #[test]
    fn reads_task_interpreters_and_expands_home() {
        let temporary = tempdir().unwrap();
        let home = temporary.path().join("home");
        let path = temporary.path().join("config.toml");
        let interpreters = task_interpreters_from_config(
            &path,
            r#"format_version = 2
repository = "~/dotfiles"

[runners.python]
command = "~/.venvs/wombat/bin/python"
args = ["-I"]
"#,
            &home,
        )
        .unwrap();
        let python = &interpreters["python"];
        assert_eq!(
            python.command(),
            Some(
                home.join(".venvs/wombat/bin/python")
                    .to_string_lossy()
                    .as_ref()
            )
        );
        assert_eq!(python.args(), ["-I"]);
    }

    #[test]
    fn recording_a_source_preserves_comments_and_tables() {
        let existing = "# my wombat config\nformat_version = 2\nrepository = \"~/old\"\n\n# venv\n[runners.python]\ncommand = \"python3\"\n";
        let updated = rewrite_repository(existing, "/srv/dotfiles");
        assert!(updated.contains("# my wombat config"));
        assert!(updated.contains("repository = \"/srv/dotfiles\""));
        assert!(!updated.contains("~/old"));
        assert!(updated.contains("# venv"));
        assert!(updated.contains("[runners.python]"));
    }

    #[test]
    fn recording_a_source_inserts_above_any_table() {
        let existing = "format_version = 2\n[runners.python]\ncommand = \"python3\"\n";
        let updated = rewrite_repository(existing, "/srv/dotfiles");
        let repository = updated.find("repository =").expect("key is inserted");
        let table = updated.find("[runners.python]").expect("table is kept");
        assert!(
            repository < table,
            "a top-level key must precede any table: {updated}"
        );
    }

    #[test]
    fn rejects_unknown_and_relative_task_interpreters() {
        let temporary = tempdir().unwrap();
        let home = temporary.path().join("home");
        let path = temporary.path().join("config.toml");
        let unknown = task_interpreters_from_config(
            &path,
            "format_version = 2\nrepository = \"~/dotfiles\"\n[runners.ruby]\ncommand = \"ruby\"\n",
            &home,
        )
        .unwrap_err();
        assert!(
            unknown
                .to_string()
                .contains("unknown task interpreter `ruby`")
        );

        let relative = task_interpreters_from_config(
            &path,
            "format_version = 2\nrepository = \"~/dotfiles\"\n[runners.python]\ncommand = \"venv/bin/python\"\n",
            &home,
        )
        .unwrap_err();
        assert!(
            relative
                .to_string()
                .contains("must be a bare command, absolute path")
        );
    }
}
