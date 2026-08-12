use std::fs;
use std::path::Path;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::manifest::{ArtifactPolicy, SourceFile, UnallocatedPolicy};
use crate::presentation::LogLevel;
use crate::{Result, WombatError};

const PROJECT_FORMAT_VERSION: u32 = 2;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectConfig {
    format_version: u32,
    #[serde(default)]
    artifacts: ArtifactConfig,
    #[serde(default)]
    log: LogConfig,
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

pub(crate) fn load(root: &Path) -> Result<(ArtifactPolicy, LogLevel, Option<SourceFile>)> {
    let path = root.join("wombat.toml");
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((ArtifactPolicy::default(), LogLevel::Warn, None));
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
    let unallocated = match config.artifacts.unallocated {
        ConfiguredUnallocated::Ignore => UnallocatedPolicy::Ignore,
        ConfiguredUnallocated::Warn => UnallocatedPolicy::Warn,
        ConfiguredUnallocated::Error => UnallocatedPolicy::Error,
    };
    let log = LogLevel::parse(&config.log.level).ok_or_else(|| WombatError::configuration(format!("repository `wombat.toml` log.level must be debug, info, notice, warn, or error; got `{}`", config.log.level)))?;
    Ok((
        ArtifactPolicy { unallocated },
        log,
        Some(SourceFile {
            path: "wombat.toml".to_string(),
            digest: format!(
                "sha256:{}",
                Sha256::digest(&bytes)
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            ),
        }),
    ))
}
