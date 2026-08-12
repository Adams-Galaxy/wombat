//! RAII file locks for workspace and target-state transactions.

use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};

use crate::{Result, WombatError};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Mode {
    Shared,
    Exclusive,
}

pub(crate) struct Guard {
    file: File,
    path: PathBuf,
}

impl Guard {
    pub(crate) fn try_acquire(file: File, path: &Path, mode: Mode) -> Result<Self> {
        let result = match mode {
            Mode::Shared => file.try_lock_shared(),
            Mode::Exclusive => file.try_lock(),
        };
        result.map_err(|error| match error {
            TryLockError::WouldBlock => WombatError::conflict(format!(
                "state at `{}` is in use by another Wombat process",
                path.display()
            )),
            TryLockError::Error(error) => WombatError::io(path, error),
        })?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

impl std::fmt::Debug for Guard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Guard")
            .field("path", &self.path)
            .finish()
    }
}
