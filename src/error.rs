use std::io;
use std::path::PathBuf;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, WombatError>;

#[derive(Debug, Error)]
pub enum WombatError {
    #[error("{0}")]
    Configuration(String),

    #[error("failed to access `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error(transparent)]
    Lua(#[from] mlua::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl WombatError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    pub(crate) fn configuration(message: impl Into<String>) -> Self {
        Self::Configuration(message.into())
    }
}
