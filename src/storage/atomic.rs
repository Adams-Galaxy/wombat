//! Durable atomic replacement for JSON state.

use std::io::Write;
use std::path::Path;

use serde::Serialize;
use tempfile::NamedTempFile;

use crate::storage::{path, permissions};
use crate::{Result, WombatError};

pub(crate) fn write_json_pretty<T: Serialize>(
    destination: &Path,
    value: &T,
    private: bool,
) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    write_bytes(destination, &bytes, private)
}

pub(crate) fn write_bytes(destination: &Path, bytes: &[u8], private: bool) -> Result<()> {
    let parent = path::parent(destination)?;
    let mut temporary =
        NamedTempFile::new_in(parent).map_err(|error| WombatError::io(parent, error))?;
    if private {
        permissions::set_private_file(temporary.as_file(), temporary.path())?;
    }
    temporary
        .write_all(bytes)
        .map_err(|error| WombatError::io(temporary.path(), error))?;
    temporary
        .as_file_mut()
        .sync_all()
        .map_err(|error| WombatError::io(temporary.path(), error))?;
    temporary
        .persist(destination)
        .map_err(|error| WombatError::io(destination, error.error))?;
    sync_directory(parent)
}

pub(crate) fn sync_directory(path: &Path) -> Result<()> {
    std::fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| WombatError::io(path, error))
}
