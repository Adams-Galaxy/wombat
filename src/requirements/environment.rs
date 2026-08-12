//! Process-wide serialization for host package-manager observations and mutation.

use super::*;

pub(super) struct EnvironmentLock {
    file: File,
}

impl EnvironmentLock {
    pub(super) fn shared() -> Result<Self> {
        Self::acquire(false)
    }

    pub(super) fn exclusive() -> Result<Self> {
        Self::acquire(true)
    }

    fn acquire(exclusive: bool) -> Result<Self> {
        let root = environment_state_root()?;
        fs::create_dir_all(&root).map_err(|error| WombatError::io(&root, error))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
                .map_err(|error| WombatError::io(&root, error))?;
        }
        let path = root.join("environment.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| WombatError::io(&path, error))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|error| WombatError::io(&path, error))?;
        }
        let result = if exclusive {
            file.try_lock()
        } else {
            file.try_lock_shared()
        };
        result.map_err(|error| match error {
            TryLockError::WouldBlock => WombatError::configuration(
                "another Wombat check or bootstrap owns the local environment lock",
            ),
            TryLockError::Error(error) => WombatError::io(&path, error),
        })?;
        Ok(Self { file })
    }
}

impl Drop for EnvironmentLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn environment_state_root() -> Result<PathBuf> {
    if let Some(root) = env::var_os("XDG_STATE_HOME") {
        let root = PathBuf::from(root);
        if !root.is_absolute() {
            return Err(WombatError::configuration(
                "XDG_STATE_HOME must be absolute",
            ));
        }
        return Ok(root.join("wombat"));
    }
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| WombatError::configuration("HOME is not set"))?;
    Ok(home.join(".local/state/wombat"))
}

#[cfg(test)]
mod tests {
    use super::providers::{first_version, version_at_least};

    #[test]
    fn loose_versions_are_explicitly_monotonic() {
        assert!(version_at_least("ripgrep 15.2.0", "14.0"));
        assert!(!version_at_least("0.10.4", "0.11.0"));
        assert_eq!(
            first_version("tool version 2.3.4\n").as_deref(),
            Some("2.3.4")
        );
    }
}
