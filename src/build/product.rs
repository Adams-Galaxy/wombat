//! Verified product handles and relocation-safe snapshots.
//!
//! Deployments consume a stable product view. A normal workspace supplies that
//! stability through a shared lock; a relocated product without workspace
//! metadata is copied and verified on both sides of the copy instead.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use super::publication::ensure_plain_file;
use super::validation::verify_product;
use super::workspace::acquire_build_lock;
use crate::model::manifest::Manifest;
use crate::{Result, WombatError};

#[derive(Clone, Debug, PartialEq)]
pub struct VerifiedBuild {
    pub build_dir: PathBuf,
    pub manifest: Manifest,
}

#[derive(Debug)]
pub struct OpenedBuild {
    pub requested_build_dir: PathBuf,
    pub product_dir: PathBuf,
    pub manifest: Manifest,
    _lock: Option<crate::storage::locking::Guard>,
    _snapshot: Option<tempfile::TempDir>,
}

/// Verify a published product against its manifest.
///
/// Checks format and construction versions, identity, and that the tree on disk
/// is exactly what the manifest describes. A product that fails this is not
/// deployed.
pub fn verify_build(build_dir: &Path) -> Result<VerifiedBuild> {
    let build_dir =
        fs::canonicalize(build_dir).map_err(|error| WombatError::io(build_dir, error))?;
    let lock_path = build_dir.join(".wombat/lock");
    let _lock = match fs::symlink_metadata(&lock_path) {
        Ok(_) => {
            ensure_plain_file(&lock_path)?;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&lock_path)
                .map_err(|error| WombatError::io(&lock_path, error))?;
            Some(acquire_build_lock(
                file,
                &lock_path,
                &build_dir,
                crate::storage::locking::Mode::Shared,
            )?)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(WombatError::io(&lock_path, error)),
    };
    let manifest = verify_product(&build_dir)?;
    Ok(VerifiedBuild {
        build_dir,
        manifest,
    })
}

/// Open a verified product, holding a shared lock for the handle's lifetime.
pub fn open_build(build_dir: &Path) -> Result<OpenedBuild> {
    let requested_build_dir =
        fs::canonicalize(build_dir).map_err(|error| WombatError::io(build_dir, error))?;
    let lock_path = requested_build_dir.join(".wombat/lock");
    match fs::symlink_metadata(&lock_path) {
        Ok(_) => {
            ensure_plain_file(&lock_path)?;
            let lock = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&lock_path)
                .map_err(|error| WombatError::io(&lock_path, error))?;
            let lock = acquire_build_lock(
                lock,
                &lock_path,
                &requested_build_dir,
                crate::storage::locking::Mode::Shared,
            )?;
            let manifest = verify_product(&requested_build_dir)?;
            Ok(OpenedBuild {
                requested_build_dir: requested_build_dir.clone(),
                product_dir: requested_build_dir,
                manifest,
                _lock: Some(lock),
                _snapshot: None,
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let before = verify_product(&requested_build_dir)?;
            let snapshot = tempfile::tempdir().map_err(|error| {
                WombatError::io(std::env::temp_dir().join("wombat-build-snapshot"), error)
            })?;
            copy_functional_product(&requested_build_dir, snapshot.path())?;
            let manifest = verify_product(snapshot.path())?;
            let after = verify_product(&requested_build_dir)?;
            if before.build_id != manifest.build_id || after.build_id != manifest.build_id {
                return Err(WombatError::configuration(format!(
                    "relocated build product `{}` changed while it was being opened",
                    requested_build_dir.display()
                )));
            }
            Ok(OpenedBuild {
                requested_build_dir,
                product_dir: snapshot.path().to_path_buf(),
                manifest,
                _lock: None,
                _snapshot: Some(snapshot),
            })
        }
        Err(error) => Err(WombatError::io(&lock_path, error)),
    }
}

fn copy_functional_product(source: &Path, destination: &Path) -> Result<()> {
    let manifest = source.join("manifest.json");
    ensure_plain_file(&manifest)?;
    fs::copy(&manifest, destination.join("manifest.json"))
        .map_err(|error| WombatError::io(&manifest, error))?;
    copy_product_directory(&source.join("tree"), &destination.join("tree"))?;
    for name in ["providers", "scripts"] {
        let directory = source.join(name);
        if directory
            .try_exists()
            .map_err(|error| WombatError::io(&directory, error))?
        {
            copy_product_directory(&directory, &destination.join(name))?;
        }
    }
    Ok(())
}

fn copy_product_directory(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source).map_err(|error| WombatError::io(source, error))?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(WombatError::configuration(format!(
            "build product directory `{}` must be a non-symlink directory",
            source.display()
        )));
    }
    fs::create_dir(destination).map_err(|error| WombatError::io(destination, error))?;
    let mut entries = fs::read_dir(source)
        .map_err(|error| WombatError::io(source, error))?
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|error| WombatError::io(source, error))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path)
            .map_err(|error| WombatError::io(&source_path, error))?;
        if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
            copy_product_directory(&source_path, &destination_path)?;
        } else if metadata.file_type().is_file() && !metadata.file_type().is_symlink() {
            fs::copy(&source_path, &destination_path)
                .map_err(|error| WombatError::io(&source_path, error))?;
            fs::set_permissions(&destination_path, metadata.permissions())
                .map_err(|error| WombatError::io(&destination_path, error))?;
        } else {
            return Err(WombatError::configuration(format!(
                "build product entry `{}` must be a regular file or directory",
                source_path.display()
            )));
        }
    }
    Ok(())
}
