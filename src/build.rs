use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::{Builder, NamedTempFile};

use crate::manifest::{
    Artifact, EvaluatedArtifact, FileContent, MANIFEST_FORMAT_VERSION, Manifest, TargetAnchor,
    TargetOrigin,
};
use crate::path::{display_target, parse_explicit_target, validate_relative_path};
use crate::runtime::evaluate;
use crate::{Result, WombatError};

const WORKSPACE_FORMAT_VERSION: u32 = 1;
const WOMBAT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildOptions {
    pub source_root: PathBuf,
    pub build_dir: PathBuf,
}

impl BuildOptions {
    pub fn new(source_root: impl Into<PathBuf>, build_dir: impl Into<PathBuf>) -> Self {
        Self {
            source_root: source_root.into(),
            build_dir: build_dir.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuildStatus {
    Created,
    Updated,
    Unchanged,
    Repaired,
}

impl fmt::Display for BuildStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Created => "created",
            Self::Updated => "updated",
            Self::Unchanged => "unchanged",
            Self::Repaired => "repaired",
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct BuildOutcome {
    pub status: BuildStatus,
    pub build_dir: PathBuf,
    pub build_id: String,
    pub artifact_count: usize,
    pub manifest: Manifest,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VerifiedBuild {
    pub build_dir: PathBuf,
    pub manifest: Manifest,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceMarker {
    format_version: u32,
    source_root: String,
}

#[derive(Serialize)]
struct IdentityPayload<'a> {
    format_version: u32,
    wombat_version: &'a str,
    modules: &'a [crate::manifest::ManifestModule],
    dependencies: &'a [crate::manifest::Dependency],
    artifacts: &'a [Artifact],
}

enum CurrentProduct {
    Missing,
    Valid(Manifest),
    Invalid,
}

pub fn build(options: BuildOptions) -> Result<BuildOutcome> {
    let source_root = fs::canonicalize(&options.source_root)
        .map_err(|error| WombatError::io(&options.source_root, error))?;
    let requested_build = if options.build_dir.is_absolute() {
        options.build_dir
    } else {
        source_root.join(options.build_dir)
    };
    let build_dir = resolve_maybe_missing(&requested_build)?;
    validate_build_location(&source_root, &build_dir)?;

    prepare_workspace_directory(&build_dir)?;
    let internal = build_dir.join(".wombat");
    ensure_plain_directory(&internal)?;
    let lock_path = internal.join("lock");
    ensure_plain_file_or_missing(&lock_path)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| WombatError::io(&lock_path, error))?;
    acquire_exclusive(&lock, &build_dir)?;
    ensure_workspace_marker(&build_dir, &source_root)?;
    recover_publication(&build_dir)?;

    let desired = evaluate(&source_root)?;
    let staging_root = internal.join("staging");
    ensure_plain_directory(&staging_root)?;
    clear_directory_contents(&staging_root)?;
    let staging = Builder::new()
        .prefix("build-")
        .tempdir_in(&staging_root)
        .map_err(|error| WombatError::io(&staging_root, error))?;
    let manifest = materialise(&source_root, staging.path(), desired)?;
    let staged = verify_product(staging.path())?;
    debug_assert_eq!(staged, manifest);

    let current = inspect_product(&build_dir);
    if let CurrentProduct::Valid(existing) = &current
        && existing.build_id == manifest.build_id
    {
        return Ok(outcome(BuildStatus::Unchanged, build_dir, existing.clone()));
    }
    let status = match current {
        CurrentProduct::Missing => BuildStatus::Created,
        CurrentProduct::Valid(_) => BuildStatus::Updated,
        CurrentProduct::Invalid => BuildStatus::Repaired,
    };

    publish(&build_dir, staging.path())?;
    let published = verify_product(&build_dir)?;
    Ok(outcome(status, build_dir, published))
}

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
            acquire_shared(&file, &build_dir)?;
            Some(file)
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

fn outcome(status: BuildStatus, build_dir: PathBuf, manifest: Manifest) -> BuildOutcome {
    BuildOutcome {
        status,
        build_dir,
        build_id: manifest.build_id.clone(),
        artifact_count: manifest.artifacts.len(),
        manifest,
    }
}

fn acquire_exclusive(file: &File, build_dir: &Path) -> Result<()> {
    file.try_lock().map_err(|error| match error {
        TryLockError::WouldBlock => WombatError::configuration(format!(
            "build directory `{}` is in use by another process",
            build_dir.display()
        )),
        TryLockError::Error(error) => WombatError::io(build_dir.join(".wombat/lock"), error),
    })
}

fn acquire_shared(file: &File, build_dir: &Path) -> Result<()> {
    file.try_lock_shared().map_err(|error| match error {
        TryLockError::WouldBlock => WombatError::configuration(format!(
            "build directory `{}` is in use by another process",
            build_dir.display()
        )),
        TryLockError::Error(error) => WombatError::io(build_dir.join(".wombat/lock"), error),
    })
}

fn prepare_workspace_directory(build_dir: &Path) -> Result<()> {
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

fn ensure_workspace_marker(build_dir: &Path, source_root: &Path) -> Result<()> {
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

fn validate_build_location(source_root: &Path, build_dir: &Path) -> Result<()> {
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
        && ["modules", "lua", "home", "dot_config"]
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

fn resolve_maybe_missing(path: &Path) -> Result<PathBuf> {
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

fn materialise(
    source_root: &Path,
    product_root: &Path,
    desired: crate::manifest::EvaluatedManifest,
) -> Result<Manifest> {
    let tree = product_root.join("tree");
    fs::create_dir(&tree).map_err(|error| WombatError::io(&tree, error))?;
    for anchor in ["home", "config"] {
        let path = tree.join(anchor);
        fs::create_dir(&path).map_err(|error| WombatError::io(&path, error))?;
    }

    let mut artifacts = Vec::with_capacity(desired.artifacts.len());
    for artifact in desired.artifacts {
        artifacts.push(materialise_artifact(source_root, &tree, artifact)?);
    }
    let mut manifest = Manifest {
        format_version: MANIFEST_FORMAT_VERSION,
        wombat_version: WOMBAT_VERSION.to_string(),
        build_id: String::new(),
        modules: desired.modules,
        dependencies: desired.dependencies,
        artifacts,
    };
    manifest.build_id = compute_build_id(&manifest)?;
    write_manifest(&product_root.join("manifest.json"), &manifest)?;
    Ok(manifest)
}

fn materialise_artifact(
    source_root: &Path,
    tree: &Path,
    artifact: EvaluatedArtifact,
) -> Result<Artifact> {
    let source_path = source_root.join(&artifact.source);
    reject_source_symlinks(source_root, &source_path)?;
    let anchor = match artifact.target.anchor {
        TargetAnchor::Home => "home",
        TargetAnchor::Config => "config",
    };
    let destination = tree.join(anchor).join(&artifact.target.path);
    let parent = destination.parent().expect("file artifacts have a parent");
    fs::create_dir_all(parent).map_err(|error| WombatError::io(parent, error))?;
    let content = copy_and_hash(&source_path, &destination)?;
    Ok(Artifact {
        kind: artifact.kind,
        source: artifact.source,
        target: artifact.target,
        content,
        owner: artifact.owner,
        declared_from: artifact.declared_from,
    })
}

fn copy_and_hash(source: &Path, destination: &Path) -> Result<FileContent> {
    copy_and_hash_with_hook(source, destination, || {})
}

fn copy_and_hash_with_hook(
    source: &Path,
    destination: &Path,
    after_copy: impl FnOnce(),
) -> Result<FileContent> {
    let mut input = File::open(source).map_err(|error| WombatError::io(source, error))?;
    let before = input
        .metadata()
        .map_err(|error| WombatError::io(source, error))?;
    if !before.file_type().is_file() {
        return Err(WombatError::configuration(format!(
            "static artifact source `{}` is not a regular file",
            source.display()
        )));
    }
    let executable = executable_intent(&before);
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)
        .map_err(|error| WombatError::io(destination, error))?;
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = input
            .read(&mut buffer)
            .map_err(|error| WombatError::io(source, error))?;
        if count == 0 {
            break;
        }
        output
            .write_all(&buffer[..count])
            .map_err(|error| WombatError::io(destination, error))?;
        hasher.update(&buffer[..count]);
        size = size
            .checked_add(u64::try_from(count).expect("buffer lengths fit in u64"))
            .ok_or_else(|| WombatError::configuration("artifact size exceeds u64"))?;
    }
    after_copy();
    let after = input
        .metadata()
        .map_err(|error| WombatError::io(source, error))?;
    let path_after =
        fs::symlink_metadata(source).map_err(|error| WombatError::io(source, error))?;
    if !same_source(&before, &after) || !same_source(&before, &path_after) {
        return Err(WombatError::configuration(format!(
            "static artifact source `{}` changed during materialisation",
            source.display()
        )));
    }
    set_normalized_permissions(&output, executable, destination)?;
    output
        .sync_all()
        .map_err(|error| WombatError::io(destination, error))?;
    Ok(FileContent {
        digest: digest_string(hasher.finalize()),
        size,
        executable,
    })
}

#[cfg(unix)]
fn executable_intent(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable_intent(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn set_normalized_permissions(file: &File, executable: bool, path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(if executable {
        0o755
    } else {
        0o644
    }))
    .map_err(|error| WombatError::io(path, error))
}

#[cfg(not(unix))]
fn set_normalized_permissions(_file: &File, _executable: bool, _path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn same_source(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.mode() == right.mode()
}

#[cfg(not(unix))]
fn same_source(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
        && left.permissions().readonly() == right.permissions().readonly()
}

fn reject_source_symlinks(root: &Path, source: &Path) -> Result<()> {
    let relative = source.strip_prefix(root).map_err(|_| {
        WombatError::configuration(format!(
            "static artifact source `{}` escapes the repository",
            source.display()
        ))
    })?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
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

fn compute_build_id(manifest: &Manifest) -> Result<String> {
    let payload = IdentityPayload {
        format_version: manifest.format_version,
        wombat_version: &manifest.wombat_version,
        modules: &manifest.modules,
        dependencies: &manifest.dependencies,
        artifacts: &manifest.artifacts,
    };
    let bytes = serde_json::to_vec(&payload)?;
    Ok(digest_string(Sha256::digest(bytes)))
}

fn write_manifest(path: &Path, manifest: &Manifest) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(manifest)?;
    bytes.push(b'\n');
    write_bytes(path, &bytes)
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    let parent = path.parent().expect("workspace files have parents");
    let mut temporary =
        NamedTempFile::new_in(parent).map_err(|error| WombatError::io(parent, error))?;
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

fn write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(|error| WombatError::io(path, error))?;
    file.write_all(bytes)
        .map_err(|error| WombatError::io(path, error))?;
    file.sync_all()
        .map_err(|error| WombatError::io(path, error))
}

fn verify_product(root: &Path) -> Result<Manifest> {
    let manifest_path = root.join("manifest.json");
    ensure_plain_file(&manifest_path)?;
    let contents = fs::read_to_string(&manifest_path)
        .map_err(|error| WombatError::io(&manifest_path, error))?;
    let manifest: Manifest = serde_json::from_str(&contents)?;
    validate_manifest(&manifest)?;
    let expected_id = compute_build_id(&manifest)?;
    if manifest.build_id != expected_id {
        return Err(WombatError::configuration(format!(
            "build ID mismatch in `{}`: recorded `{}`, computed `{expected_id}`",
            manifest_path.display(),
            manifest.build_id
        )));
    }
    verify_tree(&root.join("tree"), &manifest)?;
    Ok(manifest)
}

fn validate_manifest(manifest: &Manifest) -> Result<()> {
    if manifest.format_version != MANIFEST_FORMAT_VERSION {
        return Err(WombatError::configuration(format!(
            "unsupported manifest format version {}; expected {MANIFEST_FORMAT_VERSION}",
            manifest.format_version
        )));
    }
    if manifest.wombat_version != WOMBAT_VERSION {
        return Err(WombatError::configuration(format!(
            "build was produced by Wombat {}, but this is Wombat {WOMBAT_VERSION}",
            manifest.wombat_version
        )));
    }
    if !manifest
        .modules
        .windows(2)
        .all(|pair| pair[0].name < pair[1].name)
    {
        return Err(WombatError::configuration(
            "manifest modules are not uniquely sorted",
        ));
    }
    if !manifest
        .dependencies
        .windows(2)
        .all(|pair| pair[0] < pair[1])
    {
        return Err(WombatError::configuration(
            "manifest dependencies are not uniquely sorted",
        ));
    }
    if !manifest.artifacts.windows(2).all(|pair| {
        pair[0]
            .target
            .key()
            .cmp(&pair[1].target.key())
            .then_with(|| pair[0].owner.cmp(&pair[1].owner))
            .then_with(|| pair[0].source.cmp(&pair[1].source))
            .then_with(|| pair[0].declared_from.cmp(&pair[1].declared_from))
            .is_lt()
    }) {
        return Err(WombatError::configuration(
            "manifest artifacts are not uniquely sorted",
        ));
    }
    for artifact in &manifest.artifacts {
        validate_relative_path(&artifact.source, "manifest artifact source")?;
        validate_relative_path(&artifact.target.path, "manifest target path")?;
        let expected_display = display_target(artifact.target.anchor, &artifact.target.path);
        if artifact.target.display != expected_display {
            return Err(WombatError::configuration(format!(
                "manifest target display `{}` does not match `{expected_display}`",
                artifact.target.display
            )));
        }
        match &artifact.target.origin {
            TargetOrigin::Explicit { declared } => {
                let parsed = parse_explicit_target(declared)?;
                if parsed.anchor != artifact.target.anchor
                    || parsed.path != artifact.target.path
                    || parsed.display != artifact.target.display
                {
                    return Err(WombatError::configuration(format!(
                        "manifest explicit target `{declared}` does not match its resolved target"
                    )));
                }
            }
            TargetOrigin::Inferred { source_anchor, .. } => {
                if source_anchor.target_anchor() != artifact.target.anchor {
                    return Err(WombatError::configuration(format!(
                        "manifest inferred target `{}` has an incompatible source anchor",
                        artifact.target.display
                    )));
                }
            }
        }
    }
    Ok(())
}

fn verify_tree(tree: &Path, manifest: &Manifest) -> Result<()> {
    let mut expected_files = BTreeMap::new();
    let mut expected_dirs = BTreeSet::from(["home".to_string(), "config".to_string()]);
    for artifact in &manifest.artifacts {
        let anchor = match artifact.target.anchor {
            TargetAnchor::Home => "home",
            TargetAnchor::Config => "config",
        };
        let relative = format!("{anchor}/{}", artifact.target.path);
        if expected_files.insert(relative.clone(), artifact).is_some() {
            return Err(WombatError::configuration(format!(
                "manifest contains duplicate tree path `{relative}`"
            )));
        }
        let mut parent = Path::new(&relative).parent();
        while let Some(path) = parent {
            if path.as_os_str().is_empty() {
                break;
            }
            expected_dirs.insert(path.to_string_lossy().replace('\\', "/"));
            parent = path.parent();
        }
    }
    let metadata = fs::symlink_metadata(tree).map_err(|error| WombatError::io(tree, error))?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(WombatError::configuration(format!(
            "build tree `{}` must be a non-symlink directory",
            tree.display()
        )));
    }
    let mut seen_files = BTreeSet::new();
    let mut seen_dirs = BTreeSet::new();
    walk_tree(
        tree,
        tree,
        &expected_files,
        &expected_dirs,
        &mut seen_files,
        &mut seen_dirs,
    )?;
    if seen_files.len() != expected_files.len() || seen_dirs != expected_dirs {
        return Err(WombatError::configuration(
            "build tree is missing manifest-required entries",
        ));
    }
    Ok(())
}

fn walk_tree(
    root: &Path,
    directory: &Path,
    expected_files: &BTreeMap<String, &Artifact>,
    expected_dirs: &BTreeSet<String>,
    seen_files: &mut BTreeSet<String>,
    seen_dirs: &mut BTreeSet<String>,
) -> Result<()> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| WombatError::io(directory, error))?
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|error| WombatError::io(directory, error))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .expect("walked entries remain under root");
        let relative = relative
            .to_str()
            .ok_or_else(|| {
                WombatError::configuration(format!(
                    "build tree entry `{}` is not valid UTF-8",
                    path.display()
                ))
            })?
            .replace('\\', "/");
        let metadata =
            fs::symlink_metadata(&path).map_err(|error| WombatError::io(&path, error))?;
        if metadata.file_type().is_symlink() {
            return Err(WombatError::configuration(format!(
                "build tree entry `{relative}` must not be a symbolic link"
            )));
        }
        if metadata.file_type().is_dir() {
            if !expected_dirs.contains(&relative) {
                return Err(WombatError::configuration(format!(
                    "build tree contains extra directory `{relative}`"
                )));
            }
            seen_dirs.insert(relative);
            walk_tree(
                root,
                &path,
                expected_files,
                expected_dirs,
                seen_files,
                seen_dirs,
            )?;
        } else if metadata.file_type().is_file() {
            let artifact = expected_files.get(&relative).ok_or_else(|| {
                WombatError::configuration(format!("build tree contains extra file `{relative}`"))
            })?;
            verify_file(&path, artifact)?;
            seen_files.insert(relative);
        } else {
            return Err(WombatError::configuration(format!(
                "build tree entry `{relative}` is not a regular file or directory"
            )));
        }
    }
    Ok(())
}

fn verify_file(path: &Path, artifact: &Artifact) -> Result<()> {
    let mut file = File::open(path).map_err(|error| WombatError::io(path, error))?;
    let metadata = file
        .metadata()
        .map_err(|error| WombatError::io(path, error))?;
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
            .checked_add(u64::try_from(count).expect("buffer lengths fit in u64"))
            .ok_or_else(|| WombatError::configuration("artifact size exceeds u64"))?;
    }
    let digest = digest_string(hasher.finalize());
    if size != artifact.content.size || digest != artifact.content.digest {
        return Err(WombatError::configuration(format!(
            "build tree file `{}` does not match its manifest content identity",
            path.display()
        )));
    }
    if executable_intent(&metadata) != artifact.content.executable {
        return Err(WombatError::configuration(format!(
            "build tree file `{}` has incorrect executable intent",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let expected = if artifact.content.executable {
            0o755
        } else {
            0o644
        };
        if metadata.permissions().mode() & 0o777 != expected {
            return Err(WombatError::configuration(format!(
                "build tree file `{}` has mode {:o}, expected {expected:o}",
                path.display(),
                metadata.permissions().mode() & 0o777
            )));
        }
    }
    Ok(())
}

fn inspect_product(root: &Path) -> CurrentProduct {
    let manifest = root.join("manifest.json");
    let tree = root.join("tree");
    let manifest_exists = manifest.try_exists().unwrap_or(false);
    let tree_exists = tree.try_exists().unwrap_or(false);
    if !manifest_exists && !tree_exists {
        CurrentProduct::Missing
    } else {
        match verify_product(root) {
            Ok(manifest) => CurrentProduct::Valid(manifest),
            Err(_) => CurrentProduct::Invalid,
        }
    }
}

fn recover_publication(build_dir: &Path) -> Result<()> {
    let rollback = build_dir.join(".wombat/rollback");
    if !rollback
        .try_exists()
        .map_err(|error| WombatError::io(&rollback, error))?
    {
        return Ok(());
    }
    ensure_plain_directory(&rollback)?;
    if verify_product(build_dir).is_ok() {
        remove_entry(&rollback)?;
        return Ok(());
    }
    if verify_product(&rollback).is_ok() {
        remove_reserved_product(build_dir)?;
        restore_rollback(build_dir, &rollback)?;
    } else {
        remove_entry(&rollback)?;
    }
    Ok(())
}

fn publish(build_dir: &Path, staged: &Path) -> Result<()> {
    publish_with_hook(build_dir, staged, |_| Ok(()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PublicationStep {
    BeforeBackup,
    PreviousBackedUp,
    TreePublished,
    ManifestPublished,
}

fn publish_with_hook(
    build_dir: &Path,
    staged: &Path,
    mut after_step: impl FnMut(PublicationStep) -> Result<()>,
) -> Result<()> {
    let rollback = build_dir.join(".wombat/rollback");
    remove_entry_if_exists(&rollback)?;
    fs::create_dir(&rollback).map_err(|error| WombatError::io(&rollback, error))?;
    let mut product_was_mutated = false;
    let result = (|| {
        after_step(PublicationStep::BeforeBackup)?;
        for name in ["tree", "manifest.json"] {
            let current = build_dir.join(name);
            if current
                .try_exists()
                .map_err(|error| WombatError::io(&current, error))?
            {
                fs::rename(&current, rollback.join(name))
                    .map_err(|error| WombatError::io(&current, error))?;
                product_was_mutated = true;
            }
        }
        after_step(PublicationStep::PreviousBackedUp)?;
        fs::rename(staged.join("tree"), build_dir.join("tree"))
            .map_err(|error| WombatError::io(build_dir.join("tree"), error))?;
        product_was_mutated = true;
        after_step(PublicationStep::TreePublished)?;
        fs::rename(
            staged.join("manifest.json"),
            build_dir.join("manifest.json"),
        )
        .map_err(|error| WombatError::io(build_dir.join("manifest.json"), error))?;
        after_step(PublicationStep::ManifestPublished)?;
        verify_product(build_dir)?;
        Ok(())
    })();
    if let Err(error) = result {
        if product_was_mutated {
            remove_reserved_product(build_dir)?;
            restore_rollback(build_dir, &rollback)?;
        } else {
            remove_entry(&rollback)?;
        }
        return Err(error);
    }
    remove_entry(&rollback)
}

fn restore_rollback(build_dir: &Path, rollback: &Path) -> Result<()> {
    for name in ["tree", "manifest.json"] {
        let source = rollback.join(name);
        if source
            .try_exists()
            .map_err(|error| WombatError::io(&source, error))?
        {
            fs::rename(&source, build_dir.join(name))
                .map_err(|error| WombatError::io(&source, error))?;
        }
    }
    remove_entry_if_exists(rollback)
}

fn remove_reserved_product(build_dir: &Path) -> Result<()> {
    remove_entry_if_exists(&build_dir.join("manifest.json"))?;
    remove_entry_if_exists(&build_dir.join("tree"))
}

fn clear_directory_contents(directory: &Path) -> Result<()> {
    for entry in fs::read_dir(directory).map_err(|error| WombatError::io(directory, error))? {
        let entry = entry.map_err(|error| WombatError::io(directory, error))?;
        remove_entry(&entry.path())?;
    }
    Ok(())
}

fn ensure_plain_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            Ok(())
        }
        Ok(_) => Err(WombatError::configuration(format!(
            "workspace path `{}` must be a non-symlink directory",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(|error| WombatError::io(path, error))
        }
        Err(error) => Err(WombatError::io(path, error)),
    }
}

fn ensure_plain_file_or_missing(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => ensure_plain_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(WombatError::io(path, error)),
    }
}

fn ensure_plain_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| WombatError::io(path, error))?;
    if metadata.file_type().is_file() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(WombatError::configuration(format!(
            "workspace path `{}` must be a regular non-symlink file",
            path.display()
        )))
    }
}

fn remove_entry_if_exists(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => remove_entry(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(WombatError::io(path, error)),
    }
}

fn remove_entry(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| WombatError::io(path, error))?;
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path).map_err(|error| WombatError::io(path, error))
    } else {
        fs::remove_file(path).map_err(|error| WombatError::io(path, error))
    }
}

fn digest_string(bytes: impl AsRef<[u8]>) -> String {
    let mut output = String::with_capacity(7 + bytes.as_ref().len() * 2);
    output.push_str("sha256:");
    for byte in bytes.as_ref() {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to a string cannot fail");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repository(root: &Path) {
        fs::create_dir_all(root.join("modules/dot_config")).unwrap();
        fs::create_dir_all(root.join("dot_config")).unwrap();
        fs::write(
            root.join("wombat.lua"),
            "local w = require(\"wombat\")\nw.use(\"app\")\n",
        )
        .unwrap();
        fs::write(
            root.join("modules/dot_config/app.lua"),
            "local w = require(\"wombat\")\nw.install(\"app.toml\")\n",
        )
        .unwrap();
        fs::write(root.join("dot_config/app.toml"), "version = 1\n").unwrap();
    }

    #[test]
    fn source_mutation_during_copy_is_rejected() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let destination = temporary.path().join("destination");
        fs::write(&source, "before\n").unwrap();

        let error = copy_and_hash_with_hook(&source, &destination, || {
            fs::write(&source, "changed while materialising\n").unwrap();
        })
        .unwrap_err()
        .to_string();

        assert!(error.contains("changed during materialisation"), "{error}");
    }

    #[test]
    fn every_publication_transition_restores_the_previous_product_on_failure() {
        for failure_step in [
            PublicationStep::BeforeBackup,
            PublicationStep::PreviousBackedUp,
            PublicationStep::TreePublished,
            PublicationStep::ManifestPublished,
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let source = temporary.path().join("repository");
            let current = temporary.path().join("current");
            let staged = temporary.path().join("staged");
            repository(&source);
            let previous = build(BuildOptions::new(&source, &current)).unwrap();
            fs::write(source.join("dot_config/app.toml"), "version = 2\n").unwrap();
            let replacement = build(BuildOptions::new(&source, &staged)).unwrap();
            assert_ne!(previous.build_id, replacement.build_id);

            let error = publish_with_hook(&current, &staged, |step| {
                if step == failure_step {
                    Err(WombatError::configuration(format!(
                        "injected failure after {step:?}"
                    )))
                } else {
                    Ok(())
                }
            })
            .unwrap_err()
            .to_string();

            assert!(error.contains("injected failure"), "{error}");
            let restored = verify_product(&current).unwrap();
            assert_eq!(restored.build_id, previous.build_id);
            assert!(!current.join(".wombat/rollback").exists());
        }
    }
}
