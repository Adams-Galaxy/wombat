//! Typed repository locators and safe Git acquisition.
//!
//! Shorthand expansion is deliberately narrow: a bare owner becomes that owner's
//! GitHub `dotfiles`, `owner/name` becomes GitHub HTTPS, and `--ssh` changes
//! only that expansion. Anything explicit — HTTPS, SSH, `git+`, `file://`, a
//! local path — is used exactly as written.
//!
//! Acquisition reuses an existing checkout only when its origin matches, and
//! never pulls, switches branches, changes remotes, or cleans a working tree.
//! Setup runs on machines where the user may already have work in progress, and
//! quietly moving their repository underneath them is not recoverable.
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::Builder;
use url::Url;

use crate::{Result, WombatError};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepositoryIdentity {
    Network { host: String, path: String },
    Local(PathBuf),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryLocator {
    pub clone_url: String,
    pub identity: RepositoryIdentity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AcquisitionStatus {
    Cloned,
    Reused,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcquisitionOutcome {
    pub status: AcquisitionStatus,
    pub destination: PathBuf,
    pub locator: RepositoryLocator,
}

impl RepositoryLocator {
    pub fn parse(value: &str, ssh_shorthand: bool) -> Result<Self> {
        if value.is_empty() || value.starts_with('-') || value.chars().any(char::is_control) {
            return Err(WombatError::configuration(format!(
                "invalid repository locator `{value}`"
            )));
        }
        if value.starts_with('/') || value.starts_with("./") || value.starts_with("../") {
            return Self::local(Path::new(value));
        }
        if value == "~" || value.starts_with("~/") {
            let home = env::var_os("HOME").ok_or_else(|| {
                WombatError::configuration("HOME is not set; cannot expand local repository path")
            })?;
            let path = if value == "~" {
                PathBuf::from(home)
            } else {
                PathBuf::from(home).join(&value[2..])
            };
            return Self::local(&path);
        }
        if let Some(value) = value
            .strip_prefix("git+https://")
            .map(|rest| format!("https://{rest}"))
            .or_else(|| {
                value
                    .strip_prefix("git+ssh://")
                    .map(|rest| format!("ssh://{rest}"))
            })
        {
            return Self::network_url(&value);
        }
        if value.starts_with("https://") || value.starts_with("ssh://") {
            return Self::network_url(value);
        }
        if value.starts_with("file://") {
            let url = Url::parse(value).map_err(|error| {
                WombatError::configuration(format!(
                    "invalid file repository locator `{value}`: {error}"
                ))
            })?;
            if url.query().is_some() || url.fragment().is_some() {
                return Err(WombatError::configuration(
                    "file repository locators cannot contain a query or fragment",
                ));
            }
            let path = url.to_file_path().map_err(|_| {
                WombatError::configuration(format!(
                    "file repository locator `{value}` is not a local path"
                ))
            })?;
            let canonical =
                fs::canonicalize(&path).map_err(|error| WombatError::io(&path, error))?;
            return Ok(Self {
                clone_url: value.to_string(),
                identity: RepositoryIdentity::Local(canonical),
            });
        }
        if let Some((authority, path)) = value.split_once(':')
            && authority.contains('@')
            && !path.is_empty()
        {
            let (_, host) = authority.rsplit_once('@').ok_or_else(|| {
                WombatError::configuration(format!("invalid SCP repository locator `{value}`"))
            })?;
            return Self::network_identity(value, host, path);
        }

        let components = value.split('/').collect::<Vec<_>>();
        if !(components.len() == 1 || components.len() == 2)
            || components
                .iter()
                .any(|component| !valid_github_component(component))
            || components[0].contains('.')
        {
            return Err(WombatError::configuration(format!(
                "ambiguous repository locator `{value}`; use owner, owner/repository, an explicit URL, or ./local/path"
            )));
        }
        let owner = components[0];
        let repository = components.get(1).copied().unwrap_or("dotfiles");
        let clone_url = if ssh_shorthand {
            format!("git@github.com:{owner}/{repository}.git")
        } else {
            format!("https://github.com/{owner}/{repository}.git")
        };
        Self::network_identity(&clone_url, "github.com", &format!("{owner}/{repository}"))
    }

    fn local(path: &Path) -> Result<Self> {
        let canonical = fs::canonicalize(path).map_err(|error| WombatError::io(path, error))?;
        Ok(Self {
            clone_url: canonical.to_string_lossy().into_owned(),
            identity: RepositoryIdentity::Local(canonical),
        })
    }

    fn network_url(value: &str) -> Result<Self> {
        let url = Url::parse(value).map_err(|error| {
            WombatError::configuration(format!("invalid repository locator `{value}`: {error}"))
        })?;
        if url.query().is_some() || url.fragment().is_some() {
            return Err(WombatError::configuration(
                "repository URLs cannot contain a query or fragment",
            ));
        }
        if url.scheme() == "https" && (!url.username().is_empty() || url.password().is_some()) {
            return Err(WombatError::configuration(
                "HTTPS repository locators must not embed credentials",
            ));
        }
        let host = url.host_str().ok_or_else(|| {
            WombatError::configuration(format!("repository URL `{value}` has no host"))
        })?;
        let clone_url = value.to_string();
        Self::network_identity(&clone_url, host, url.path())
    }

    fn network_identity(clone_url: &str, host: &str, path: &str) -> Result<Self> {
        let path = path.trim_start_matches('/').trim_end_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path);
        if host.is_empty() || path.is_empty() || path.split('/').any(str::is_empty) {
            return Err(WombatError::configuration(format!(
                "repository locator `{clone_url}` has an empty host or path"
            )));
        }
        Ok(Self {
            clone_url: clone_url.to_string(),
            identity: RepositoryIdentity::Network {
                host: host.to_ascii_lowercase(),
                path: path.to_string(),
            },
        })
    }
}

pub fn acquire_repository(
    locator: RepositoryLocator,
    destination: &Path,
) -> Result<AcquisitionOutcome> {
    if destination.exists() {
        let metadata = fs::symlink_metadata(destination)
            .map_err(|error| WombatError::io(destination, error))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(WombatError::configuration(format!(
                "repository destination `{}` must be a plain directory",
                destination.display()
            )));
        }
        if directory_is_empty(destination)? {
            fs::remove_dir(destination).map_err(|error| WombatError::io(destination, error))?;
        } else {
            let origin = git_output(destination, &["config", "--get", "remote.origin.url"])?;
            let origin = origin.trim();
            if origin.is_empty() {
                return Err(WombatError::configuration(format!(
                    "existing repository destination `{}` has no remote.origin.url",
                    destination.display()
                )));
            }
            let existing = RepositoryLocator::parse(origin, false).map_err(|error| {
                error.with_note(format!(
                    "while normalizing existing origin `{origin}` in `{}`",
                    destination.display()
                ))
            })?;
            if existing.identity != locator.identity {
                return Err(WombatError::configuration(format!(
                    "existing repository `{}` has origin `{origin}`, which does not match `{}`",
                    destination.display(),
                    locator.clone_url
                )));
            }
            return Ok(AcquisitionOutcome {
                status: AcquisitionStatus::Reused,
                destination: destination.to_path_buf(),
                locator,
            });
        }
    }

    let parent = destination.parent().ok_or_else(|| {
        WombatError::configuration(format!(
            "repository destination `{}` has no parent",
            destination.display()
        ))
    })?;
    fs::create_dir_all(parent).map_err(|error| WombatError::io(parent, error))?;
    let staging = Builder::new()
        .prefix(".wombat-clone-")
        .tempdir_in(parent)
        .map_err(|error| WombatError::io(parent, error))?;
    let checkout = staging.path().join("source");
    let git = command_path("git")?;
    let status = Command::new(&git)
        .args(["clone", "--"])
        .arg(&locator.clone_url)
        .arg(&checkout)
        .status()
        .map_err(|error| WombatError::io(&git, error))?;
    if !status.success() {
        return Err(WombatError::configuration(format!(
            "Git clone of `{}` failed with {status}",
            locator.clone_url
        )));
    }
    fs::rename(&checkout, destination).map_err(|error| WombatError::io(destination, error))?;
    Ok(AcquisitionOutcome {
        status: AcquisitionStatus::Cloned,
        destination: destination.to_path_buf(),
        locator,
    })
}

fn valid_github_component(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn directory_is_empty(path: &Path) -> Result<bool> {
    Ok(fs::read_dir(path)
        .map_err(|error| WombatError::io(path, error))?
        .next()
        .is_none())
}

fn command_path(name: &str) -> Result<PathBuf> {
    env::split_paths(&env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join(name))
        .find(|path| path.is_file())
        .ok_or_else(|| WombatError::configuration(format!("`{name}` is not available on PATH")))
}

fn git_output(repository: &Path, args: &[&str]) -> Result<String> {
    let git = command_path("git")?;
    let output = Command::new(&git)
        .arg("-C")
        .arg(repository)
        .args(args)
        .output()
        .map_err(|error| WombatError::io(&git, error))?;
    if !output.status.success() {
        return Err(WombatError::configuration(format!(
            "`git {}` failed in `{}`",
            args.join(" "),
            repository.display()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::{RepositoryIdentity, RepositoryLocator};

    #[test]
    fn normalizes_github_and_explicit_network_locators() {
        let owner = RepositoryLocator::parse("Adams-Galaxy", false).unwrap();
        assert_eq!(
            owner.clone_url,
            "https://github.com/Adams-Galaxy/dotfiles.git"
        );
        let ssh = RepositoryLocator::parse("Adams-Galaxy/dotfiles", true).unwrap();
        assert_eq!(ssh.clone_url, "git@github.com:Adams-Galaxy/dotfiles.git");
        assert_eq!(owner.identity, ssh.identity);
        let marked =
            RepositoryLocator::parse("git+https://github.com/Adams-Galaxy/dotfiles.git", false)
                .unwrap();
        assert_eq!(marked.identity, owner.identity);
        assert!(matches!(owner.identity, RepositoryIdentity::Network { .. }));
    }

    #[test]
    fn rejects_credentials_options_and_ambiguous_hosts() {
        assert!(RepositoryLocator::parse("https://token@github.com/owner/repo", false).is_err());
        assert!(RepositoryLocator::parse("--upload-pack=evil", false).is_err());
        assert!(RepositoryLocator::parse("github.com/owner/repo", false).is_err());
    }
}
