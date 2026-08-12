use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::{Result, WombatError};

const CACHE_FORMAT_VERSION: u32 = 1;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CachedOutput {
    pub relative: String,
    pub digest: String,
    pub size: u64,
    pub executable: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskDerivation {
    format_version: u32,
    outputs: Vec<CachedOutput>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TemplateDerivation {
    format_version: u32,
    digest: String,
    size: u64,
}

pub(crate) struct BuildCache {
    root: PathBuf,
}

impl BuildCache {
    pub(crate) fn open(build_dir: &Path) -> Result<Self> {
        let root = build_dir.join(".wombat/cache");
        for path in [
            root.clone(),
            root.join("derivations"),
            root.join("derivations/templates"),
            root.join("derivations/tasks"),
            root.join("blobs"),
            root.join("blobs/sha256"),
        ] {
            ensure_private_directory(&path)?;
        }
        Ok(Self { root })
    }

    pub(crate) fn key(&self, namespace: &str, value: &impl Serialize) -> Result<String> {
        let bytes = serde_json::to_vec(&(namespace, value))?;
        Ok(hex_digest(&bytes))
    }

    pub(crate) fn load_template(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let path = self.derivation("templates", key);
        let Some(record): Option<TemplateDerivation> = read_json_or_miss(&path)? else {
            return Ok(None);
        };
        if record.format_version != CACHE_FORMAT_VERSION {
            return Ok(None);
        }
        self.load_blob(&record.digest, record.size)
    }

    pub(crate) fn store_template(&self, key: &str, bytes: &[u8]) -> Result<()> {
        let digest = digest(bytes);
        self.store_blob(&digest, bytes)?;
        write_json_atomic(
            &self.derivation("templates", key),
            &TemplateDerivation {
                format_version: CACHE_FORMAT_VERSION,
                digest,
                size: u64::try_from(bytes.len())
                    .map_err(|_| WombatError::configuration("cache blob exceeds u64"))?,
            },
        )
    }

    pub(crate) fn load_task(&self, key: &str, output: &Path) -> Result<Option<Vec<CachedOutput>>> {
        let path = self.derivation("tasks", key);
        let Some(record): Option<TaskDerivation> = read_json_or_miss(&path)? else {
            return Ok(None);
        };
        if record.format_version != CACHE_FORMAT_VERSION {
            return Ok(None);
        }
        let mut previous = None;
        let mut blobs = Vec::with_capacity(record.outputs.len());
        for item in &record.outputs {
            if crate::path::validate_relative_path(&item.relative, "cached task output").is_err()
                || previous.is_some_and(|value: &str| value >= item.relative.as_str())
            {
                return Ok(None);
            }
            let Some(bytes) = self.load_blob(&item.digest, item.size)? else {
                return Ok(None);
            };
            blobs.push((item, bytes));
            previous = Some(item.relative.as_str());
        }
        for (item, bytes) in blobs {
            let destination = item
                .relative
                .split('/')
                .fold(output.to_path_buf(), |path, part| path.join(part));
            let parent = destination.expect_parent()?;
            fs::create_dir_all(parent).map_err(|error| WombatError::io(parent, error))?;
            write_file(&destination, &bytes, item.executable)?;
        }
        Ok(Some(record.outputs))
    }

    pub(crate) fn store_task(
        &self,
        key: &str,
        outputs: &[CachedOutput],
        root: &Path,
    ) -> Result<()> {
        for output in outputs {
            let path = output
                .relative
                .split('/')
                .fold(root.to_path_buf(), |path, part| path.join(part));
            let bytes = fs::read(&path).map_err(|error| WombatError::io(&path, error))?;
            if digest(&bytes) != output.digest
                || u64::try_from(bytes.len()).ok() != Some(output.size)
            {
                return Err(WombatError::configuration(format!(
                    "task output `{}` changed while populating the cache",
                    output.relative
                )));
            }
            self.store_blob(&output.digest, &bytes)?;
        }
        write_json_atomic(
            &self.derivation("tasks", key),
            &TaskDerivation {
                format_version: CACHE_FORMAT_VERSION,
                outputs: outputs.to_vec(),
            },
        )
    }

    fn derivation(&self, kind: &str, key: &str) -> PathBuf {
        self.root
            .join("derivations")
            .join(kind)
            .join(format!("{key}.json"))
    }

    fn load_blob(&self, expected_digest: &str, size: u64) -> Result<Option<Vec<u8>>> {
        let Some(hex) = expected_digest.strip_prefix("sha256:") else {
            return Ok(None);
        };
        let path = self.root.join("blobs/sha256").join(hex);
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(WombatError::io(&path, error)),
        };
        let metadata = file
            .metadata()
            .map_err(|error| WombatError::io(&path, error))?;
        if !metadata.file_type().is_file() || metadata.len() != size {
            return Ok(None);
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|error| WombatError::io(&path, error))?;
        if digest(&bytes) != expected_digest {
            return Ok(None);
        }
        Ok(Some(bytes))
    }

    fn store_blob(&self, digest: &str, bytes: &[u8]) -> Result<()> {
        let hex = digest
            .strip_prefix("sha256:")
            .expect("locally computed digests are SHA-256");
        let path = self.root.join("blobs/sha256").join(hex);
        if path
            .try_exists()
            .map_err(|error| WombatError::io(&path, error))?
        {
            if self
                .load_blob(
                    digest,
                    u64::try_from(bytes.len())
                        .map_err(|_| WombatError::configuration("cache blob exceeds u64"))?,
                )?
                .is_some()
            {
                return Ok(());
            }
            fs::remove_file(&path).map_err(|error| WombatError::io(&path, error))?;
        }
        let parent = path.expect_parent()?;
        let mut temporary =
            NamedTempFile::new_in(parent).map_err(|error| WombatError::io(parent, error))?;
        set_private_file(temporary.as_file(), temporary.path())?;
        temporary
            .write_all(bytes)
            .map_err(|error| WombatError::io(temporary.path(), error))?;
        temporary
            .as_file_mut()
            .sync_all()
            .map_err(|error| WombatError::io(temporary.path(), error))?;
        temporary
            .persist(&path)
            .map_err(|error| WombatError::io(&path, error.error))?;
        Ok(())
    }
}

fn read_json_or_miss<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(WombatError::io(path, error)),
    };
    Ok(serde_json::from_slice(&bytes).ok())
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.expect_parent()?;
    let bytes = serde_json::to_vec(value)?;
    let mut temporary =
        NamedTempFile::new_in(parent).map_err(|error| WombatError::io(parent, error))?;
    set_private_file(temporary.as_file(), temporary.path())?;
    temporary
        .write_all(&bytes)
        .map_err(|error| WombatError::io(temporary.path(), error))?;
    temporary
        .as_file_mut()
        .sync_all()
        .map_err(|error| WombatError::io(temporary.path(), error))?;
    temporary
        .persist(path)
        .map_err(|error| WombatError::io(path, error.error))?;
    Ok(())
}

fn write_file(path: &Path, bytes: &[u8], executable: bool) -> Result<()> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(|error| WombatError::io(path, error))?;
    file.write_all(bytes)
        .map_err(|error| WombatError::io(path, error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(fs::Permissions::from_mode(if executable {
            0o755
        } else {
            0o644
        }))
        .map_err(|error| WombatError::io(path, error))?;
    }
    file.sync_all()
        .map_err(|error| WombatError::io(path, error))
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    crate::storage::permissions::ensure_private_directory(path)
}

fn set_private_file(file: &File, path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| WombatError::io(path, error))?;
    }
    Ok(())
}

fn digest(bytes: &[u8]) -> String {
    crate::storage::digest::sha256(bytes)
}

fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to a string cannot fail");
    }
    output
}

trait ParentPath {
    fn expect_parent(&self) -> Result<&Path>;
}

impl ParentPath for Path {
    fn expect_parent(&self) -> Result<&Path> {
        self.parent().ok_or_else(|| {
            WombatError::configuration(format!("path `{}` has no parent", self.display()))
        })
    }
}
