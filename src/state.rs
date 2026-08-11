use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::manifest::{
    Artifact, ArtifactKind, EvaluatedTargetOrigin, FileContent, Production, SourceOrigin,
    SourceTrace, TargetPath, Task, TaskCachePolicy, TaskLogPolicy, TaskOutput, TaskRunner,
    TaskRunnerFamily, TaskTargetRoot,
};
use crate::{Result, WombatError};

pub(crate) const TARGET_STATE_FORMAT_VERSION: u32 = 2;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TargetState {
    pub format_version: u32,
    pub target_home: String,
    pub complete_build_id: Option<String>,
    pub artifacts: Vec<AppliedArtifact>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AppliedArtifact {
    pub kind: ArtifactKind,
    pub source: String,
    pub source_origin: SourceOrigin,
    pub production: Production,
    pub target: TargetPath,
    pub content: FileContent,
    pub mode: u32,
    pub owner: String,
    pub declared_at: SourceTrace,
}

impl AppliedArtifact {
    pub fn from_artifact(artifact: Artifact) -> Self {
        let mode = if artifact.content.executable {
            0o755
        } else {
            0o644
        };
        Self {
            kind: artifact.kind,
            source: artifact.source,
            source_origin: artifact.source_origin,
            production: artifact.production,
            target: artifact.target,
            content: artifact.content,
            mode,
            owner: artifact.owner,
            declared_at: artifact.declared_at,
        }
    }

    pub fn to_artifact(&self) -> Artifact {
        Artifact {
            kind: self.kind,
            source: self.source.clone(),
            source_origin: self.source_origin.clone(),
            production: self.production.clone(),
            target: self.target.clone(),
            content: self.content.clone(),
            owner: self.owner.clone(),
            declared_at: self.declared_at.clone(),
        }
    }
}

impl TargetState {
    pub fn empty(target_home: String) -> Self {
        Self {
            format_version: TARGET_STATE_FORMAT_VERSION,
            target_home,
            complete_build_id: None,
            artifacts: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LockMode {
    Shared,
    Exclusive,
}

#[derive(Debug)]
pub(crate) struct TargetStateGuard {
    directory: PathBuf,
    state_path: PathBuf,
    target_home: String,
    _lock: File,
}

impl Drop for TargetStateGuard {
    fn drop(&mut self) {
        let _ = File::unlock(&self._lock);
    }
}

impl TargetStateGuard {
    pub fn open(state_root: &Path, target_home: &Path, mode: LockMode) -> Result<Self> {
        let target_home =
            fs::canonicalize(target_home).map_err(|error| WombatError::io(target_home, error))?;
        let target_home_string = target_home
            .to_str()
            .ok_or_else(|| WombatError::configuration("target home must be valid UTF-8"))?
            .to_string();
        let key = target_key(&target_home);
        let wombat_root = state_root.join("wombat");
        let targets_root = wombat_root.join("targets");
        let directory = targets_root.join(key);
        for path in [&wombat_root, &targets_root, &directory] {
            create_private_directories(path)?;
        }
        let lock_path = directory.join("lock");
        match fs::symlink_metadata(&lock_path) {
            Ok(metadata)
                if metadata.file_type().is_symlink() || !metadata.file_type().is_file() =>
            {
                return Err(WombatError::configuration(format!(
                    "target state lock `{}` must be a regular non-symlink file",
                    lock_path.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(WombatError::io(&lock_path, error)),
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|error| WombatError::io(&lock_path, error))?;
        set_private_file_permissions(&lock, &lock_path)?;
        let result = match mode {
            LockMode::Shared => lock.try_lock_shared(),
            LockMode::Exclusive => lock.try_lock(),
        };
        result.map_err(|error| match error {
            TryLockError::WouldBlock => WombatError::configuration(format!(
                "target `{target_home_string}` is in use by another Wombat process"
            )),
            TryLockError::Error(error) => WombatError::io(&lock_path, error),
        })?;
        Ok(Self {
            state_path: directory.join("state.json"),
            directory,
            target_home: target_home_string,
            _lock: lock,
        })
    }

    pub fn load(&self) -> Result<TargetState> {
        match fs::symlink_metadata(&self.state_path) {
            Ok(metadata)
                if metadata.file_type().is_symlink() || !metadata.file_type().is_file() =>
            {
                return Err(WombatError::configuration(format!(
                    "target state `{}` must be a regular non-symlink file",
                    self.state_path.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(TargetState::empty(self.target_home.clone()));
            }
            Err(error) => return Err(WombatError::io(&self.state_path, error)),
        }
        let contents = match fs::read_to_string(&self.state_path) {
            Ok(contents) => contents,
            Err(error) => return Err(WombatError::io(&self.state_path, error)),
        };
        let state: TargetState = serde_json::from_str(&contents)?;
        if state.format_version != TARGET_STATE_FORMAT_VERSION {
            return Err(WombatError::configuration(format!(
                "unsupported target state format version {} in `{}`; expected {TARGET_STATE_FORMAT_VERSION}",
                state.format_version,
                self.state_path.display()
            )));
        }
        if state.target_home != self.target_home {
            return Err(WombatError::configuration(format!(
                "target state `{}` belongs to `{}`, not `{}`",
                self.state_path.display(),
                state.target_home,
                self.target_home
            )));
        }
        if !state
            .artifacts
            .windows(2)
            .all(|pair| pair[0].target.key().cmp(&pair[1].target.key()).is_lt())
        {
            return Err(WombatError::configuration(format!(
                "target state artifacts in `{}` are not uniquely sorted",
                self.state_path.display()
            )));
        }
        validate_state_artifacts(&state.artifacts)?;
        if let Some(build_id) = &state.complete_build_id
            && !valid_digest(build_id)
        {
            return Err(WombatError::configuration(format!(
                "target state complete build ID in `{}` is invalid",
                self.state_path.display()
            )));
        }
        Ok(state)
    }

    pub fn write(&self, state: &TargetState) -> Result<()> {
        if state.target_home != self.target_home {
            return Err(WombatError::configuration(
                "refusing to write target state for a different target home",
            ));
        }
        if state.format_version != TARGET_STATE_FORMAT_VERSION {
            return Err(WombatError::configuration(
                "refusing to write an unsupported target state version",
            ));
        }
        if state
            .complete_build_id
            .as_deref()
            .is_some_and(|build_id| !valid_digest(build_id))
        {
            return Err(WombatError::configuration(
                "refusing to write an invalid complete build ID",
            ));
        }
        validate_state_artifacts(&state.artifacts)?;
        let mut bytes = serde_json::to_vec_pretty(state)?;
        bytes.push(b'\n');
        let mut temporary = NamedTempFile::new_in(&self.directory)
            .map_err(|error| WombatError::io(&self.directory, error))?;
        set_private_file_permissions(temporary.as_file(), temporary.path())?;
        temporary
            .write_all(&bytes)
            .map_err(|error| WombatError::io(temporary.path(), error))?;
        temporary
            .as_file_mut()
            .sync_all()
            .map_err(|error| WombatError::io(temporary.path(), error))?;
        temporary
            .persist(&self.state_path)
            .map_err(|error| WombatError::io(&self.state_path, error.error))?;
        sync_directory(&self.directory)
    }

    #[cfg(test)]
    pub fn state_path(&self) -> &Path {
        &self.state_path
    }
}

pub(crate) fn resolve_state_root(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(explicit) = explicit {
        if !explicit.is_absolute() {
            return Err(WombatError::configuration(
                "explicit target state root must be an absolute path",
            ));
        }
        return Ok(explicit.to_path_buf());
    }
    if let Some(value) = std::env::var_os("XDG_STATE_HOME") {
        let value = PathBuf::from(value);
        if !value.is_absolute() {
            return Err(WombatError::configuration(
                "XDG_STATE_HOME must be an absolute path",
            ));
        }
        return Ok(value);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| WombatError::configuration("HOME is not set; cannot resolve state root"))?;
    if !home.is_absolute() {
        return Err(WombatError::configuration("HOME must be an absolute path"));
    }
    Ok(home.join(".local/state"))
}

fn target_key(target_home: &Path) -> String {
    let digest = Sha256::digest(target_home.as_os_str().as_encoded_bytes());
    let mut output = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to a string cannot fail");
    }
    output
}

fn create_private_directories(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(|error| WombatError::io(path, error))?;
    let metadata = fs::symlink_metadata(path).map_err(|error| WombatError::io(path, error))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        return Err(WombatError::configuration(format!(
            "target state directory `{}` must be a non-symlink directory",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| WombatError::io(path, error))?;
    }
    Ok(())
}

fn validate_state_artifacts(artifacts: &[AppliedArtifact]) -> Result<()> {
    for artifact in artifacts {
        let expected_mode = if artifact.content.executable {
            0o755
        } else {
            0o644
        };
        if artifact.mode != expected_mode {
            return Err(WombatError::configuration(format!(
                "target state artifact `{}` has mode {:04o}, expected {expected_mode:04o}",
                artifact.target.display, artifact.mode
            )));
        }
    }
    let tasks = state_tasks(artifacts);
    let manifest = crate::manifest::Manifest {
        format_version: crate::manifest::MANIFEST_FORMAT_VERSION,
        wombat_version: env!("CARGO_PKG_VERSION").to_string(),
        build_id: String::new(),
        plan_id: format!("sha256:{}", "0".repeat(64)),
        sources: artifacts
            .iter()
            .map(|artifact| artifact.declared_at.primary.source.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .map(|path| crate::manifest::SourceFile {
                path,
                digest: format!("sha256:{}", "0".repeat(64)),
            })
            .collect(),
        inputs: Vec::new(),
        target: crate::context::ResolvedTarget {
            platform: crate::context::TargetPlatform::minimal(
                crate::context::OperatingSystemName::Macos,
                crate::context::Architecture::Aarch64,
            ),
            origin: crate::context::TargetOrigin::HostDefault,
            declared_at: None,
        },
        observations: Vec::new(),
        modules: Vec::new(),
        dependencies: Vec::new(),
        build_providers: Vec::new(),
        build_requirements: Vec::new(),
        build_preparations: Vec::new(),
        providers: Vec::new(),
        requirements: Vec::new(),
        preparations: Vec::new(),
        tasks,
        artifacts: artifacts.iter().map(AppliedArtifact::to_artifact).collect(),
    };
    crate::build::validate_manifest(&manifest)
}

fn state_tasks(artifacts: &[AppliedArtifact]) -> Vec<Task> {
    let mut tasks = std::collections::BTreeMap::<String, Task>::new();
    for artifact in artifacts {
        let Production::Task {
            identity, output, ..
        } = &artifact.production
        else {
            continue;
        };
        let task = tasks.entry(identity.clone()).or_insert_with(|| Task {
            identity: identity.clone(),
            owner: artifact.owner.clone(),
            entrypoint: "tasks/target-state-placeholder".to_string(),
            entrypoint_digest: format!("sha256:{}", "0".repeat(64)),
            params: crate::frozen::FrozenValue::empty_map(),
            runner: TaskRunner {
                contract_version: 1,
                family: TaskRunnerFamily::EmbeddedLua,
                command: None,
                args: Vec::new(),
            },
            python_helper: false,
            logs: TaskLogPolicy::Never,
            cache: TaskCachePolicy {
                enabled: false,
                revision: None,
            },
            target_root: Some(TaskTargetRoot {
                anchor: artifact.target.anchor,
                path: String::new(),
                origin: EvaluatedTargetOrigin::Explicit {
                    declared: artifact.target.display.clone(),
                },
            }),
            declared_at: artifact.declared_at.clone(),
            outputs: Vec::new(),
        });
        task.outputs.push(TaskOutput {
            relative: output.clone(),
            content: artifact.content.clone(),
        });
    }
    let mut tasks = tasks.into_values().collect::<Vec<_>>();
    for task in &mut tasks {
        task.outputs
            .sort_by(|left, right| left.relative.cmp(&right.relative));
    }
    tasks
}

fn valid_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn set_private_file_permissions(file: &File, path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| WombatError::io(path, error))?;
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    let directory = File::open(path).map_err(|error| WombatError::io(path, error))?;
    directory
        .sync_all()
        .map_err(|error| WombatError::io(path, error))
}

#[cfg(test)]
mod tests {
    use super::{LockMode, TargetStateGuard};

    #[test]
    fn state_round_trips_strictly() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let guard = TargetStateGuard::open(temporary.path(), &home, LockMode::Exclusive).unwrap();
        let state = guard.load().unwrap();
        guard.write(&state).unwrap();
        assert_eq!(guard.load().unwrap(), state);
        assert!(guard.state_path().is_file());
    }

    #[test]
    fn shared_state_locks_coexist_and_block_an_exclusive_writer() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let first = TargetStateGuard::open(temporary.path(), &home, LockMode::Shared).unwrap();
        let second = TargetStateGuard::open(temporary.path(), &home, LockMode::Shared).unwrap();
        let error = TargetStateGuard::open(temporary.path(), &home, LockMode::Exclusive)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("in use by another Wombat process"),
            "{error}"
        );
        drop(second);
        drop(first);
        TargetStateGuard::open(temporary.path(), &home, LockMode::Exclusive).unwrap();
    }
}
