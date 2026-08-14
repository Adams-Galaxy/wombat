//! Build workspace ownership, location safety, and locking.
//!
//! A workspace marker binds a build directory to one source tree. Without that
//! binding, a convenient `--clean` could erase unrelated files or combine two
//! repositories into one published product.

use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::materialisation::write_json_atomic;
use super::publication::{clear_directory_contents, ensure_plain_file};
use crate::{Result, WombatError};

const WORKSPACE_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceMarker {
    format_version: u32,
    source_root: String,
}

pub(super) fn clean_transient_workspace(build_dir: &Path) -> Result<()> {
    let internal = build_dir.join(".wombat");
    for name in ["cache", "tasks", "logs", "staging"] {
        let path = internal.join(name);
        if path.exists() {
            clear_directory_contents(&path)?;
        }
    }
    let journal = internal.join("execution-journal.json");
    if journal.exists() {
        fs::remove_file(&journal).map_err(|error| WombatError::io(&journal, error))?;
    }
    Ok(())
}

pub(super) fn acquire_build_lock(
    file: std::fs::File,
    lock_path: &Path,
    build_dir: &Path,
    mode: crate::storage::locking::Mode,
) -> Result<crate::storage::locking::Guard> {
    crate::storage::locking::Guard::try_acquire_with(
        file,
        lock_path,
        mode,
        format!(
            "build directory `{}` is in use by another process",
            build_dir.display()
        ),
    )
}

pub(super) fn prepare_workspace_directory(build_dir: &Path) -> Result<()> {
    match fs::symlink_metadata(build_dir) {
        Ok(metadata) if !metadata.file_type().is_dir() => Err(WombatError::configuration(format!(
            "build directory `{}` must be a directory",
            build_dir.display()
        ))),
        Ok(_) => {
            let entries = fs::read_dir(build_dir)
                .map_err(|error| WombatError::io(build_dir, error))?
                .collect::<std::io::Result<Vec<_>>>()
                .map_err(|error| WombatError::io(build_dir, error))?;
            let marker = build_dir.join(".wombat/workspace.json");
            let only_internal = entries.len() == 1 && entries[0].file_name() == ".wombat";
            if !entries.is_empty()
                && !marker
                    .try_exists()
                    .map_err(|error| WombatError::io(&marker, error))?
                && !only_internal
            {
                return Err(WombatError::configuration(format!(
                    "refusing nonempty unmarked build directory `{}`",
                    build_dir.display()
                )));
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(build_dir).map_err(|error| WombatError::io(build_dir, error))
        }
        Err(error) => Err(WombatError::io(build_dir, error)),
    }
}

pub(super) fn ensure_workspace_marker(build_dir: &Path, source_root: &Path) -> Result<()> {
    let marker_path = build_dir.join(".wombat/workspace.json");
    let source = source_root.to_str().ok_or_else(|| {
        WombatError::configuration("repository roots used for builds must be valid UTF-8")
    })?;
    if marker_path
        .try_exists()
        .map_err(|error| WombatError::io(&marker_path, error))?
    {
        ensure_plain_file(&marker_path)?;
        let contents = fs::read_to_string(&marker_path)
            .map_err(|error| WombatError::io(&marker_path, error))?;
        let marker: WorkspaceMarker = serde_json::from_str(&contents)?;
        if marker.format_version != WORKSPACE_FORMAT_VERSION {
            return Err(WombatError::configuration(format!(
                "unsupported build workspace format version {} in `{}`",
                marker.format_version,
                marker_path.display()
            )));
        }
        if marker.source_root != source {
            return Err(WombatError::configuration(format!(
                "build directory `{}` belongs to source `{}`, not `{source}`",
                build_dir.display(),
                marker.source_root
            )));
        }
        return Ok(());
    }
    let internal = build_dir.join(".wombat");
    if internal
        .try_exists()
        .map_err(|error| WombatError::io(&internal, error))?
    {
        let unexpected = fs::read_dir(&internal)
            .map_err(|error| WombatError::io(&internal, error))?
            .filter_map(|entry| match entry {
                Ok(entry) if entry.file_name() == "lock" => None,
                other => Some(other),
            })
            .next()
            .transpose()
            .map_err(|error| WombatError::io(&internal, error))?;
        if unexpected.is_some() {
            return Err(WombatError::configuration(format!(
                "refusing nonempty unmarked build directory `{}`",
                build_dir.display()
            )));
        }
    }
    let marker = WorkspaceMarker {
        format_version: WORKSPACE_FORMAT_VERSION,
        source_root: source.to_string(),
    };
    write_json_atomic(&marker_path, &marker)
}

pub(super) fn validate_build_location(source_root: &Path, build_dir: &Path) -> Result<()> {
    if build_dir.parent().is_none() {
        return Err(WombatError::configuration(
            "the filesystem root cannot be a build directory",
        ));
    }
    if source_root == build_dir || source_root.starts_with(build_dir) {
        return Err(WombatError::configuration(format!(
            "build directory `{}` must not be the repository or its ancestor",
            build_dir.display()
        )));
    }
    if let Some(home) = env::var_os("HOME").map(PathBuf::from)
        && let Ok(home) = fs::canonicalize(home)
        && home == build_dir
    {
        return Err(WombatError::configuration(
            "the user home cannot be a build directory",
        ));
    }
    if let Ok(relative) = build_dir.strip_prefix(source_root)
        && let Some(Component::Normal(first)) = relative.components().next()
        && [
            "modules",
            "lua",
            "tasks",
            "providers",
            "src",
            "home",
            "dot_config",
            "dot_local",
        ]
        .iter()
        .any(|reserved| first == *reserved)
    {
        return Err(WombatError::configuration(format!(
            "build directory `{}` must not be inside repository control or artifact roots",
            build_dir.display()
        )));
    }
    Ok(())
}

pub(super) fn resolve_maybe_missing(path: &Path) -> Result<PathBuf> {
    let normalized = normalize_absolute(path)?;
    let mut existing = normalized.as_path();
    let mut missing = Vec::new();
    while !existing
        .try_exists()
        .map_err(|error| WombatError::io(existing, error))?
    {
        let name = existing.file_name().ok_or_else(|| {
            WombatError::configuration(format!(
                "cannot resolve build directory `{}`",
                path.display()
            ))
        })?;
        missing.push(name.to_os_string());
        existing = existing.parent().ok_or_else(|| {
            WombatError::configuration(format!(
                "cannot resolve build directory `{}`",
                path.display()
            ))
        })?;
    }
    let mut resolved =
        fs::canonicalize(existing).map_err(|error| WombatError::io(existing, error))?;
    for name in missing.iter().rev() {
        resolved.push(name);
    }
    Ok(resolved)
}

fn normalize_absolute(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        return Err(WombatError::configuration(format!(
            "build directory `{}` did not resolve to an absolute path",
            path.display()
        )));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(WombatError::configuration(format!(
                        "build directory `{}` escapes the filesystem root",
                        path.display()
                    )));
                }
            }
        }
    }
    Ok(normalized)
}
