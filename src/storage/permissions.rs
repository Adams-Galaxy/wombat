//! Private file and directory permission policy.

use std::fs::{self, File};
use std::path::Path;

use crate::{Result, WombatError};

pub(crate) fn set_private_file(file: &File, path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| WombatError::io(path, error))?;
    }
    Ok(())
}

pub(crate) fn ensure_private_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(WombatError::policy(format!(
                "private path `{}` must be a non-symlink directory",
                path.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(|error| WombatError::io(path, error))?;
        }
        Err(error) => return Err(WombatError::io(path, error)),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| WombatError::io(path, error))?;
    }
    Ok(())
}
