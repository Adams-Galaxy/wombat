use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::{Result, WombatError};

const ROOT_SOURCE: &str = "local w = require(\"wombat\")\n\nw.use(\"auto\")\n";
const AUTO_SOURCE: &str =
    "local w = require(\"wombat\")\n\n-- wombat:add begin\n-- wombat:add end\n";
const GITIGNORE: &str = "/build/\n";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitStatus {
    Initialized,
    AlreadyInitialized,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitOutcome {
    pub root: PathBuf,
    pub status: InitStatus,
    pub warning: Option<String>,
}

impl InitOutcome {
    pub fn display(&self) -> String {
        match self.status {
            InitStatus::Initialized => {
                format!("initialized Wombat repository at {}", self.root.display())
            }
            InitStatus::AlreadyInitialized => {
                format!(
                    "Wombat repository already initialized at {}",
                    self.root.display()
                )
            }
        }
    }
}

pub fn initialize(root: &Path) -> Result<InitOutcome> {
    let root = absolute(root)?;
    preflight_root(&root)?;
    let root_file = root.join("wombat.lua");
    let modules = root.join("modules");
    let auto_file = modules.join("auto.lua");
    let gitignore = root.join(".gitignore");

    preflight_exact(&root_file, ROOT_SOURCE)?;
    preflight_directory(&modules)?;
    preflight_exact(&auto_file, AUTO_SOURCE)?;
    let warning = preflight_gitignore(&gitignore)?;

    let mut created_files = Vec::new();
    let mut created_directories = Vec::new();
    let result = (|| {
        create_directory_chain(&root, &mut created_directories)?;
        if !modules.exists() {
            fs::create_dir(&modules).map_err(|error| WombatError::io(&modules, error))?;
            created_directories.push(modules.clone());
        }
        create_file_if_missing(&root_file, ROOT_SOURCE.as_bytes(), &mut created_files)?;
        create_file_if_missing(&auto_file, AUTO_SOURCE.as_bytes(), &mut created_files)?;
        create_file_if_missing(&gitignore, GITIGNORE.as_bytes(), &mut created_files)?;
        Ok(())
    })();
    if let Err(error) = result {
        for file in created_files.iter().rev() {
            let _ = fs::remove_file(file);
        }
        for directory in created_directories.iter().rev() {
            let _ = fs::remove_dir(directory);
        }
        return Err(error);
    }
    let status = if created_files.is_empty() && created_directories.is_empty() {
        InitStatus::AlreadyInitialized
    } else {
        InitStatus::Initialized
    };
    Ok(InitOutcome {
        root,
        status,
        warning,
    })
}

fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|current| current.join(path))
            .map_err(|error| WombatError::io(".", error))
    }
}

fn preflight_root(root: &Path) -> Result<()> {
    match fs::symlink_metadata(root) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() => {
            Err(WombatError::configuration(format!(
                "initialization path `{}` must be a non-symlink directory",
                root.display()
            )))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(WombatError::io(root, error)),
    }
}

fn preflight_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() => {
            Err(WombatError::configuration(format!(
                "reserved scaffold path `{}` must be a non-symlink directory",
                path.display()
            )))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(WombatError::io(path, error)),
    }
}

fn preflight_exact(path: &Path, expected: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.file_type().is_file() => {
            Err(WombatError::configuration(format!(
                "reserved scaffold path `{}` must be a regular non-symlink file",
                path.display()
            )))
        }
        Ok(_) => {
            let actual = fs::read_to_string(path).map_err(|error| WombatError::io(path, error))?;
            if actual == expected {
                Ok(())
            } else {
                Err(WombatError::configuration(format!(
                    "reserved scaffold path `{}` already contains different content; initialization will not overwrite it",
                    path.display()
                )))
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(WombatError::io(path, error)),
    }
}

fn preflight_gitignore(path: &Path) -> Result<Option<String>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.file_type().is_file() => {
            Err(WombatError::configuration(format!(
                "existing `{}` must be a regular non-symlink file",
                path.display()
            )))
        }
        Ok(_) => {
            let contents =
                fs::read_to_string(path).map_err(|error| WombatError::io(path, error))?;
            Ok((!contents.lines().any(|line| line == "/build/")).then(|| {
                "existing .gitignore was left unchanged; add `/build/` to ignore the default workspace"
                    .to_string()
            }))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(WombatError::io(path, error)),
    }
}

fn create_directory_chain(path: &Path, created: &mut Vec<PathBuf>) -> Result<()> {
    let mut missing = Vec::new();
    let mut current = path;
    while !current.exists() {
        missing.push(current.to_path_buf());
        current = current.parent().ok_or_else(|| {
            WombatError::configuration(format!(
                "cannot find an existing parent for `{}`",
                path.display()
            ))
        })?;
    }
    let metadata =
        fs::symlink_metadata(current).map_err(|error| WombatError::io(current, error))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        return Err(WombatError::configuration(format!(
            "initialization parent `{}` must be a non-symlink directory",
            current.display()
        )));
    }
    for directory in missing.into_iter().rev() {
        fs::create_dir(&directory).map_err(|error| WombatError::io(&directory, error))?;
        created.push(directory);
    }
    Ok(())
}

fn create_file_if_missing(path: &Path, bytes: &[u8], created: &mut Vec<PathBuf>) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(|error| WombatError::io(path, error))?;
    if let Err(error) = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| WombatError::io(path, error))
    {
        let _ = fs::remove_file(path);
        return Err(error);
    }
    created.push(path.to_path_buf());
    Ok(())
}
