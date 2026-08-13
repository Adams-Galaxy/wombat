use std::fs;
use std::path::Path;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::model::manifest::{ArtifactPolicy, SourceFile, UnallocatedPolicy};
use crate::presentation::LogLevel;
use crate::{Result, WombatError};

pub mod config;
pub(crate) mod inputs;

const PROJECT_FORMAT_VERSION: u32 = 3;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectConfig {
    format_version: u32,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    artifacts: ArtifactConfig,
    #[serde(default)]
    log: LogConfig,
    #[serde(default)]
    workflow: WorkflowConfig,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowConfig {
    #[serde(default = "default_reuse")]
    reuse: bool,
    #[serde(default = "default_freshness")]
    freshness: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WorkflowPolicy {
    pub reuse: bool,
    pub freshness: std::time::Duration,
}
impl Default for WorkflowConfig {
    fn default() -> Self {
        Self {
            reuse: default_reuse(),
            freshness: default_freshness(),
        }
    }
}
fn default_reuse() -> bool {
    true
}
fn default_freshness() -> String {
    "5m".to_string()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogConfig {
    #[serde(default = "default_log_level")]
    level: String,
}
impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
        }
    }
}
fn default_log_level() -> String {
    "warn".to_string()
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactConfig {
    #[serde(default)]
    unallocated: ConfiguredUnallocated,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ConfiguredUnallocated {
    Ignore,
    #[default]
    Warn,
    Error,
}

pub(crate) struct ProjectSettings {
    pub(crate) artifact_policy: ArtifactPolicy,
    pub(crate) log_level: LogLevel,
    /// Names the persistent script state namespace. When absent the namespace
    /// follows the repository location, so relocating a checkout restarts
    /// `once` and `onchange` state.
    pub(crate) project: Option<String>,
    pub(crate) source: Option<SourceFile>,
}

pub(crate) fn load(root: &Path) -> Result<ProjectSettings> {
    let path = root.join("wombat.toml");
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ProjectSettings {
                artifact_policy: ArtifactPolicy::default(),
                log_level: LogLevel::Warn,
                project: None,
                source: None,
            });
        }
        Err(error) => return Err(WombatError::io(&path, error)),
    };
    let contents = std::str::from_utf8(&bytes).map_err(|_| {
        WombatError::configuration("repository `wombat.toml` must contain valid UTF-8")
    })?;
    let config: ProjectConfig = toml::from_str(contents).map_err(|error| {
        WombatError::configuration(format!("failed to parse repository `wombat.toml`: {error}"))
    })?;
    if config.format_version != PROJECT_FORMAT_VERSION {
        return Err(WombatError::configuration(format!(
            "unsupported repository config format version {}; expected {PROJECT_FORMAT_VERSION}",
            config.format_version
        )));
    }
    parse_freshness(&config.workflow.freshness)?;
    // Validation intentionally reads both workflow settings here. The runtime
    // consumes the persisted closure rather than trusting malformed policy.
    let _ = config.workflow.reuse;
    let unallocated = match config.artifacts.unallocated {
        ConfiguredUnallocated::Ignore => UnallocatedPolicy::Ignore,
        ConfiguredUnallocated::Warn => UnallocatedPolicy::Warn,
        ConfiguredUnallocated::Error => UnallocatedPolicy::Error,
    };
    let log = LogLevel::parse(&config.log.level).ok_or_else(|| WombatError::configuration(format!("repository `wombat.toml` log.level must be debug, info, notice, warn, or error; got `{}`", config.log.level)))?;
    let project = config.project.map(validate_project_name).transpose()?;
    Ok(ProjectSettings {
        artifact_policy: ArtifactPolicy { unallocated },
        log_level: log,
        project,
        source: Some(SourceFile {
            path: "wombat.toml".to_string(),
            digest: format!(
                "sha256:{}",
                Sha256::digest(&bytes)
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            ),
        }),
    })
}

fn validate_project_name(name: String) -> Result<String> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'));
    if valid {
        Ok(name)
    } else {
        Err(WombatError::configuration(format!(
            "repository `wombat.toml` project must be 1 to 64 characters of ASCII letters, digits, `-`, or `_`; got `{name}`"
        )))
    }
}

pub(crate) fn workflow_policy(root: &Path) -> Result<WorkflowPolicy> {
    let path = root.join("wombat.toml");
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(WorkflowPolicy {
                reuse: true,
                freshness: std::time::Duration::from_secs(300),
            });
        }
        Err(error) => return Err(WombatError::io(&path, error)),
    };
    let config: ProjectConfig = toml::from_str(&contents).map_err(|error| {
        WombatError::configuration(format!("failed to parse repository `wombat.toml`: {error}"))
    })?;
    if config.format_version != PROJECT_FORMAT_VERSION {
        return Err(WombatError::configuration(format!(
            "unsupported repository config format version {}; expected {PROJECT_FORMAT_VERSION}",
            config.format_version
        )));
    }
    Ok(WorkflowPolicy {
        reuse: config.workflow.reuse,
        freshness: parse_freshness(&config.workflow.freshness)?,
    })
}

fn parse_freshness(value: &str) -> Result<std::time::Duration> {
    let Some(unit) = value.chars().last() else {
        return Err(WombatError::configuration(
            "workflow.freshness must be a non-negative integer followed by s, m, h, or d",
        ));
    };
    let number = &value[..value.len() - unit.len_utf8()];
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(WombatError::configuration(
            "workflow.freshness must be a non-negative integer followed by s, m, h, or d",
        ));
    }
    let amount = number
        .parse::<u64>()
        .map_err(|_| WombatError::configuration("workflow.freshness is too large"))?;
    let seconds = match unit {
        's' => Some(amount),
        'm' => amount.checked_mul(60),
        'h' => amount.checked_mul(60 * 60),
        'd' => amount.checked_mul(60 * 60 * 24),
        _ => None,
    }
    .ok_or_else(|| {
        WombatError::configuration(
            "workflow.freshness must be a non-negative integer followed by s, m, h, or d",
        )
    })?;
    Ok(std::time::Duration::from_secs(seconds))
}

#[cfg(test)]
mod tests {
    use super::parse_freshness;

    #[test]
    fn freshness_accepts_one_integral_unit() {
        assert_eq!(parse_freshness("0s").unwrap().as_secs(), 0);
        assert_eq!(parse_freshness("5m").unwrap().as_secs(), 300);
        assert!(parse_freshness("1.5h").is_err());
        assert!(parse_freshness("1h30m").is_err());
    }
}
