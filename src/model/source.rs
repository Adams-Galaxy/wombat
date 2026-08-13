//! Source fingerprints and directory snapshots.
//!
//! A fingerprint is what lets a rebuild decide whether a source actually
//! changed. It combines content digest with the metadata that would change
//! meaning, so touching a file without editing it does not invalidate a product
//! and editing it always does.
//!
//! Directory snapshots are deterministic and ordered, which is what makes glob
//! and directory selection reproducible: the same tree yields the same leaves in
//! the same sequence on every machine.
use std::fs;
use std::path::{Component, Path};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::{Result, WombatError};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct DirectoryLeaf {
    pub relative: String,
    pub fingerprint: SourceFingerprint,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct SourceFingerprint {
    len: u64,
    modified: Option<SystemTime>,
    readonly: bool,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    mode: u32,
    #[cfg(unix)]
    modified_seconds: i64,
    #[cfg(unix)]
    modified_nanoseconds: i64,
}

impl SourceFingerprint {
    pub(crate) fn from_metadata(metadata: &fs::Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;

        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            readonly: metadata.permissions().readonly(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            mode: metadata.mode(),
            #[cfg(unix)]
            modified_seconds: metadata.mtime(),
            #[cfg(unix)]
            modified_nanoseconds: metadata.mtime_nsec(),
        }
    }
}

pub(crate) fn fingerprint_regular_file(path: &Path) -> Result<SourceFingerprint> {
    let metadata = fs::symlink_metadata(path).map_err(|error| WombatError::io(path, error))?;
    if metadata.file_type().is_symlink() {
        return Err(WombatError::configuration(format!(
            "static artifact source `{}` must not be a symbolic link",
            path.display()
        )));
    }
    if !metadata.file_type().is_file() {
        return Err(WombatError::configuration(format!(
            "static artifact source `{}` is not a regular file",
            path.display()
        )));
    }
    Ok(SourceFingerprint::from_metadata(&metadata))
}

pub(crate) fn validate_source_components(repository: &Path, source: &Path) -> Result<()> {
    let relative = source.strip_prefix(repository).map_err(|_| {
        WombatError::configuration(format!(
            "static artifact source `{}` escapes the repository",
            source.display()
        ))
    })?;
    let mut current = repository.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(WombatError::configuration(format!(
                "static artifact source `{}` contains an invalid path component",
                source.display()
            )));
        };
        current.push(component);
        let metadata =
            fs::symlink_metadata(&current).map_err(|error| WombatError::io(&current, error))?;
        if metadata.file_type().is_symlink() {
            return Err(WombatError::configuration(format!(
                "static artifact source `{}` must not contain symbolic links",
                source.display()
            )));
        }
    }
    Ok(())
}

pub(crate) fn snapshot_directory_filtered(
    repository: &Path,
    directory: &Path,
    mut include: impl FnMut(&str, bool) -> bool,
) -> Result<Vec<DirectoryLeaf>> {
    validate_source_components(repository, directory)?;
    let metadata =
        fs::symlink_metadata(directory).map_err(|error| WombatError::io(directory, error))?;
    if !metadata.file_type().is_dir() {
        return Err(WombatError::configuration(format!(
            "static directory source `{}` is not a directory",
            directory.display()
        )));
    }

    let mut leaves = Vec::new();
    walk_directory(directory, directory, &mut leaves, &mut include)?;
    Ok(leaves)
}

fn walk_directory(
    root: &Path,
    directory: &Path,
    leaves: &mut Vec<DirectoryLeaf>,
    include: &mut impl FnMut(&str, bool) -> bool,
) -> Result<()> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| WombatError::io(directory, error))?
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|error| WombatError::io(directory, error))?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let relative = portable_relative(root, &path)?;
        let metadata =
            fs::symlink_metadata(&path).map_err(|error| WombatError::io(&path, error))?;
        if !include(&relative, metadata.file_type().is_dir()) {
            continue;
        }
        if metadata.file_type().is_symlink() {
            return Err(WombatError::configuration(format!(
                "static directory entry `{}` must not be a symbolic link",
                path.display()
            )));
        }
        if metadata.file_type().is_dir() {
            walk_directory(root, &path, leaves, include)?;
        } else if metadata.file_type().is_file() {
            leaves.push(DirectoryLeaf {
                relative,
                fingerprint: SourceFingerprint::from_metadata(&metadata),
            });
        } else {
            return Err(WombatError::configuration(format!(
                "static directory entry `{}` is not a regular file or directory",
                path.display()
            )));
        }
    }
    leaves.sort_by(|left, right| left.relative.cmp(&right.relative));
    Ok(())
}

fn portable_relative(root: &Path, path: &Path) -> Result<String> {
    path.strip_prefix(root)
        .expect("walked directory entries remain beneath their root")
        .to_str()
        .map(|value| value.replace('\\', "/"))
        .ok_or_else(|| {
            WombatError::configuration(format!(
                "static directory entry `{}` is not valid UTF-8",
                path.display()
            ))
        })
}
