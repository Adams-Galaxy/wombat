//! Three-way reconciliation between last-applied state, the target, and the
//! desired product.
//!
//! Two-way comparison cannot tell "this changed because the configuration
//! changed" from "this changed because the user edited it". Comparing against
//! the previous applied state as well is what makes that distinction, and
//! therefore what makes safe removal and honest conflict reporting possible.
//!
//! Every artifact resolves to exactly one action. Creates, updates, adoptions,
//! and stale removals are safe and automatic; anything ambiguous becomes a
//! conflict for the caller to resolve rather than a guess made here.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::deploy::state::TargetState;
use crate::model::manifest::{Artifact, FileContent, Manifest};
use crate::model::source::SourceFingerprint;
use crate::{Result, WombatError};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReconciliationAction {
    Unchanged,
    Create,
    Adopt,
    AdvanceState,
    Update,
    Remove,
    Forget,
    Conflict,
}

impl ReconciliationAction {
    pub fn is_conflict(self) -> bool {
        self == Self::Conflict
    }

    pub fn is_safe_mutation(self) -> bool {
        matches!(self, Self::Create | Self::Update | Self::Remove)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReconciliationItem {
    pub target: String,
    pub anchor: PathBuf,
    pub path: PathBuf,
    pub action: ReconciliationAction,
    pub reason: Option<String>,
    pub desired: Option<Artifact>,
    pub previous: Option<Artifact>,
    pub(crate) actual: ActualArtifact,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReconciliationPlan {
    pub target_root: PathBuf,
    pub build_id: String,
    pub items: Vec<ReconciliationItem>,
}

impl ReconciliationPlan {
    pub fn conflicts(&self) -> impl Iterator<Item = &ReconciliationItem> {
        self.items.iter().filter(|item| item.action.is_conflict())
    }

    pub fn has_conflicts(&self) -> bool {
        self.conflicts().next().is_some()
    }

    pub fn has_differences(&self) -> bool {
        self.items
            .iter()
            .any(|item| item.action != ReconciliationAction::Unchanged)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ActualArtifact {
    Absent,
    File { content: FileContent, mode: u32 },
    Obstructed(String),
}

pub(crate) fn plan_reconciliation(
    target_root: &Path,
    desired: &Manifest,
    previous: &TargetState,
) -> Result<ReconciliationPlan> {
    let target_root =
        fs::canonicalize(target_root).map_err(|error| WombatError::io(target_root, error))?;
    let desired_by_target = desired
        .artifacts
        .iter()
        .map(|artifact| (target_key(artifact), artifact))
        .collect::<BTreeMap<_, _>>();
    let previous_artifacts = previous
        .artifacts
        .iter()
        .map(crate::deploy::state::AppliedArtifact::to_artifact)
        .collect::<Vec<_>>();
    let previous_by_target = previous_artifacts
        .iter()
        .map(|artifact| (target_key(artifact), artifact))
        .collect::<BTreeMap<_, _>>();
    let keys = desired_by_target
        .keys()
        .chain(previous_by_target.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut items = Vec::with_capacity(keys.len());

    let mut resolved = BTreeMap::new();
    for key in keys {
        let desired_artifact = desired_by_target.get(&key).copied();
        let previous_artifact = previous_by_target.get(&key).copied();
        let representative = desired_artifact
            .or(previous_artifact)
            .expect("reconciliation keys have an artifact");
        let (anchor, path) = resolve_target(&target_root, representative)?;
        if let Some(previous) = resolved.insert(path.clone(), key.clone())
            && previous != key
        {
            return Err(WombatError::configuration(format!(
                "artifact targets `{previous}` and `{key}` resolve to the same filesystem path `{}`",
                path.display()
            )));
        }
        let actual = inspect_actual(&anchor, &path)?;
        let (action, reason) = classify(desired_artifact, previous_artifact, &actual);
        items.push(ReconciliationItem {
            target: representative.target.display().to_string(),
            anchor,
            path,
            action,
            reason,
            desired: desired_artifact.cloned(),
            previous: previous_artifact.cloned(),
            actual,
        });
    }

    Ok(ReconciliationPlan {
        target_root,
        build_id: desired.build_id.clone(),
        items,
    })
}

fn classify(
    desired: Option<&Artifact>,
    previous: Option<&Artifact>,
    actual: &ActualArtifact,
) -> (ReconciliationAction, Option<String>) {
    if let ActualArtifact::Obstructed(reason) = actual {
        return (ReconciliationAction::Conflict, Some(reason.clone()));
    }
    match (desired, previous, actual) {
        (Some(_), None, ActualArtifact::Absent) => (ReconciliationAction::Create, None),
        (Some(desired), None, actual) if actual_matches(actual, desired) => {
            (ReconciliationAction::Adopt, None)
        }
        (Some(_), None, _) => conflict("an unmanaged target already exists"),

        (Some(desired), Some(previous), actual) if actual_matches(actual, desired) => {
            if artifacts_equivalent(desired, previous) {
                (ReconciliationAction::Unchanged, None)
            } else {
                (ReconciliationAction::AdvanceState, None)
            }
        }
        (Some(desired), Some(previous), actual) if actual_matches(actual, previous) => {
            if artifacts_equivalent(desired, previous) {
                (ReconciliationAction::Unchanged, None)
            } else {
                (ReconciliationAction::Update, None)
            }
        }
        (Some(_), Some(_), ActualArtifact::Absent) => {
            conflict("the previously managed target was deleted downstream")
        }
        (Some(_), Some(_), _) => conflict("the managed target was modified downstream"),

        (None, Some(_), ActualArtifact::Absent) => (ReconciliationAction::Forget, None),
        (None, Some(previous), actual) if actual_matches(actual, previous) => {
            (ReconciliationAction::Remove, None)
        }
        (None, Some(_), _) => conflict("the stale managed target was modified downstream"),
        (None, None, _) => unreachable!("unknown targets are not reconciliation keys"),
    }
}

fn conflict(reason: &str) -> (ReconciliationAction, Option<String>) {
    (ReconciliationAction::Conflict, Some(reason.to_string()))
}

fn artifacts_equivalent(left: &Artifact, right: &Artifact) -> bool {
    left == right
}

pub(crate) fn actual_matches(actual: &ActualArtifact, artifact: &Artifact) -> bool {
    matches!(
        actual,
        ActualArtifact::File { content, mode }
            if content == &artifact.content && *mode == expected_mode(artifact)
    )
}

pub(crate) fn inspect_actual(target_root: &Path, path: &Path) -> Result<ActualArtifact> {
    let relative = path.strip_prefix(target_root).map_err(|_| {
        WombatError::configuration(format!(
            "target path `{}` escapes target root `{}`",
            path.display(),
            target_root.display()
        ))
    })?;
    let mut current = target_root.to_path_buf();
    let components = relative.components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        current.push(component.as_os_str());
        let is_leaf = index + 1 == components.len();
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ActualArtifact::Absent);
            }
            Err(error) => return Err(WombatError::io(&current, error)),
        };
        if metadata.file_type().is_symlink() {
            return Ok(ActualArtifact::Obstructed(format!(
                "target path component `{}` is a symbolic link",
                current.display()
            )));
        }
        if !is_leaf && !metadata.file_type().is_dir() {
            return Ok(ActualArtifact::Obstructed(format!(
                "target path component `{}` is not a directory",
                current.display()
            )));
        }
        if is_leaf {
            if !metadata.file_type().is_file() {
                return Ok(ActualArtifact::Obstructed(format!(
                    "target `{}` is not a regular file",
                    current.display()
                )));
            }
            return read_actual_file(&current, &metadata);
        }
    }
    Ok(ActualArtifact::Obstructed(format!(
        "target path `{}` does not identify a file",
        path.display()
    )))
}

fn read_actual_file(path: &Path, before: &fs::Metadata) -> Result<ActualArtifact> {
    let fingerprint = SourceFingerprint::from_metadata(before);
    let mut file = File::open(path).map_err(|error| WombatError::io(path, error))?;
    let opened = file
        .metadata()
        .map_err(|error| WombatError::io(path, error))?;
    if SourceFingerprint::from_metadata(&opened) != fingerprint {
        return Err(target_changed(path));
    }
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| WombatError::io(path, error))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        size = size
            .checked_add(u64::try_from(count).expect("buffer length fits u64"))
            .ok_or_else(|| WombatError::configuration("target file exceeds u64"))?;
    }
    let after = file
        .metadata()
        .map_err(|error| WombatError::io(path, error))?;
    let path_after = fs::symlink_metadata(path).map_err(|error| WombatError::io(path, error))?;
    if SourceFingerprint::from_metadata(&after) != fingerprint
        || SourceFingerprint::from_metadata(&path_after) != fingerprint
    {
        return Err(target_changed(path));
    }
    Ok(ActualArtifact::File {
        content: FileContent {
            digest: digest_string(hasher.finalize()),
            size,
            executable: mode(&after) == 0o755,
        },
        mode: mode(&after),
    })
}

pub(crate) fn resolve_target(
    target_root: &Path,
    artifact: &Artifact,
) -> Result<(PathBuf, PathBuf)> {
    match artifact.target.scope {
        crate::model::manifest::TargetScope::DeploymentRoot => Ok((
            target_root.to_path_buf(),
            target_root.join(&artifact.target.path),
        )),
        crate::model::manifest::TargetScope::Absolute => external_target(&artifact.target.path),
    }
}

pub(crate) fn target_key(artifact: &Artifact) -> String {
    artifact.target.key()
}

fn external_target(path: &str) -> Result<(PathBuf, PathBuf)> {
    let destination = PathBuf::from(path);
    let leaf = destination.file_name().ok_or_else(|| {
        WombatError::configuration(format!("absolute target `{path}` must identify a file"))
    })?;
    let mut current = destination.parent().ok_or_else(|| {
        WombatError::configuration(format!("absolute target `{path}` has no parent"))
    })?;
    let mut missing = Vec::new();
    loop {
        match fs::symlink_metadata(current) {
            Ok(metadata) => {
                if !metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
                    return Err(WombatError::configuration(format!(
                        "external target parent `{}` is not a directory",
                        current.display()
                    )));
                }
                let anchor =
                    fs::canonicalize(current).map_err(|error| WombatError::io(current, error))?;
                let mut resolved = anchor.clone();
                for component in missing.iter().rev() {
                    resolved.push(component);
                }
                resolved.push(leaf);
                return Ok((anchor, resolved));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let component = current.file_name().ok_or_else(|| {
                    WombatError::configuration(format!(
                        "external target `{path}` has no existing filesystem anchor"
                    ))
                })?;
                missing.push(component.to_os_string());
                current = current.parent().ok_or_else(|| {
                    WombatError::configuration(format!(
                        "external target `{path}` has no existing filesystem anchor"
                    ))
                })?;
            }
            Err(error) => return Err(WombatError::io(current, error)),
        }
    }
}

pub(crate) fn expected_mode(artifact: &Artifact) -> u32 {
    if artifact.content.executable {
        0o755
    } else {
        0o644
    }
}

#[cfg(unix)]
fn mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    metadata.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn mode(_: &fs::Metadata) -> u32 {
    0
}

fn target_changed(path: &Path) -> WombatError {
    WombatError::configuration(format!(
        "target `{}` changed while it was being inspected",
        path.display()
    ))
}

fn digest_string(bytes: impl AsRef<[u8]>) -> String {
    crate::storage::digest::prefixed_hex(bytes)
}

#[cfg(test)]
mod tests {
    use super::{ActualArtifact, ReconciliationAction, classify, expected_mode};
    use crate::model::manifest::{
        Artifact, ArtifactKind, FileContent, Production, SourceLocation, SourceOrigin, SourceTrace,
        TargetOrigin, TargetPath,
    };

    fn artifact(owner: &str, digest: &str) -> Artifact {
        Artifact {
            kind: ArtifactKind::File,
            source: "dot_config/app".to_string(),
            source_origin: SourceOrigin::Direct {
                declared: "app".to_string(),
                expanded: "app".to_string(),
            },
            source_projection: None,
            production: Production::Static,
            target: TargetPath {
                path: "app".to_string(),
                scope: crate::model::manifest::TargetScope::DeploymentRoot,
                origin: TargetOrigin::Inferred {
                    source: "src/dot_config/app".to_string(),
                },
            },
            content: FileContent {
                digest: digest.to_string(),
                size: 1,
                executable: false,
            },
            owner: owner.to_string(),
            declared_at: SourceTrace {
                primary: SourceLocation {
                    source: format!("modules/dot_config/{owner}.lua"),
                    line: Some(1),
                    column: None,
                },
                callers: Vec::new(),
            },
        }
    }

    fn actual(artifact: &Artifact) -> ActualArtifact {
        ActualArtifact::File {
            content: artifact.content.clone(),
            mode: expected_mode(artifact),
        }
    }

    #[test]
    fn three_way_classifier_covers_every_state_transition() {
        let previous = artifact("previous", "sha256:previous");
        let desired = artifact("desired", "sha256:desired");
        let other = artifact("other", "sha256:other");

        let cases = [
            (
                Some(&desired),
                None,
                ActualArtifact::Absent,
                ReconciliationAction::Create,
            ),
            (
                Some(&desired),
                None,
                actual(&desired),
                ReconciliationAction::Adopt,
            ),
            (
                Some(&desired),
                None,
                actual(&other),
                ReconciliationAction::Conflict,
            ),
            (
                Some(&desired),
                Some(&previous),
                actual(&desired),
                ReconciliationAction::AdvanceState,
            ),
            (
                Some(&desired),
                Some(&previous),
                actual(&previous),
                ReconciliationAction::Update,
            ),
            (
                Some(&desired),
                Some(&previous),
                ActualArtifact::Absent,
                ReconciliationAction::Conflict,
            ),
            (
                Some(&desired),
                Some(&previous),
                actual(&other),
                ReconciliationAction::Conflict,
            ),
            (
                None,
                Some(&previous),
                ActualArtifact::Absent,
                ReconciliationAction::Forget,
            ),
            (
                None,
                Some(&previous),
                actual(&previous),
                ReconciliationAction::Remove,
            ),
            (
                None,
                Some(&previous),
                actual(&other),
                ReconciliationAction::Conflict,
            ),
        ];
        for (desired, previous, actual, expected) in cases {
            assert_eq!(classify(desired, previous, &actual).0, expected);
        }

        let unchanged = artifact("same", "sha256:same");
        assert_eq!(
            classify(Some(&unchanged), Some(&unchanged), &actual(&unchanged)).0,
            ReconciliationAction::Unchanged
        );

        let transferred = artifact("new-owner", "sha256:previous");
        assert_eq!(
            classify(Some(&transferred), Some(&previous), &actual(&previous)).0,
            ReconciliationAction::AdvanceState
        );
    }
}
