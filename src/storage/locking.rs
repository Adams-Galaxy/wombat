//! RAII file locks for workspace and target-state transactions.
//!
//! Every persisted-state owner locks through this module, so a second Wombat
//! process is refused rather than interleaved. Locks are advisory and held for
//! the lifetime of a [`Guard`]: acquiring one and dropping it immediately
//! protects nothing, so callers hold the guard across the whole read-decide-write
//! sequence they need to be atomic.
//!
//! Acquisition never blocks. A busy lock is a conflict the user is told about,
//! not something to wait out — a wedged Wombat would otherwise leave the next
//! invocation hanging with no explanation.

use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};

use crate::{Result, WombatError};

/// Shared locks let concurrent readers coexist; exclusive locks exclude
/// everyone.
///
/// Read-only work such as `diff` takes a shared lock so two inspections do not
/// refuse each other. Anything that writes state takes an exclusive one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Mode {
    Shared,
    Exclusive,
}

/// Holds a lock until dropped.
///
/// The guard owns the `File` because dropping the handle releases the lock on
/// every platform we support. Keep it alive for as long as the invariant needs
/// protecting, and prefer storing it in the struct that represents the
/// transaction over holding it in a local.
pub(crate) struct Guard {
    file: File,
    path: PathBuf,
}

impl Guard {
    /// Takes the lock, or reports which path is busy.
    ///
    /// `WouldBlock` becomes a conflict rather than an I/O error: another Wombat
    /// holding the lock is ordinary contention the user can act on, whereas a
    /// genuine `Error` means the filesystem could not lock at all.
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
        // Releasing is best-effort: the process is giving up the lock either
        // way, and closing the file would release it regardless.
        let _ = self.file.unlock();
    }
}

impl std::fmt::Debug for Guard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The File adds nothing a reader can act on; the path is what identifies
        // which lock this is.
        formatter
            .debug_struct("Guard")
            .field("path", &self.path)
            .finish()
    }
}
