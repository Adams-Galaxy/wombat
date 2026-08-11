use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::{Builder, NamedTempFile};

use crate::context::HostContext;
use crate::manifest::{
    Artifact, EvaluatedArtifact, EvaluatedDirectory, EvaluatedProduction, FileContent,
    MANIFEST_FORMAT_VERSION, Manifest, Production, RendererIdentity, SourceOrigin, TargetAnchor,
    TargetOrigin,
};
use crate::path::{
    display_target, expand_target_root, infer_target, parse_explicit_target,
    parse_explicit_target_root, validate_declared_source, validate_relative_path,
};
use crate::runtime::{EvaluationOptions, EvaluationOutcome, evaluate_with};
use crate::source::{SourceFingerprint, fingerprint_regular_file, snapshot_directory};
use crate::{Result, WombatError};

const WORKSPACE_FORMAT_VERSION: u32 = 1;
const WOMBAT_VERSION: &str = env!("CARGO_PKG_VERSION");
const TEMPLATE_RENDERER_NAME: &str = "handlebars";
const TEMPLATE_CONTRACT_VERSION: u32 = 1;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildOptions {
    pub source_root: PathBuf,
    pub build_dir: PathBuf,
    pub project_arguments: Vec<OsString>,
    pub host: Option<HostContext>,
}

impl BuildOptions {
    pub fn new(source_root: impl Into<PathBuf>, build_dir: impl Into<PathBuf>) -> Self {
        Self {
            source_root: source_root.into(),
            build_dir: build_dir.into(),
            project_arguments: Vec::new(),
            host: None,
        }
    }

    pub fn with_project_arguments(
        mut self,
        arguments: impl IntoIterator<Item = impl Into<OsString>>,
    ) -> Self {
        self.project_arguments = arguments.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_host(mut self, host: HostContext) -> Self {
        self.host = Some(host);
        self
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

#[derive(Debug)]
pub struct OpenedBuild {
    pub requested_build_dir: PathBuf,
    pub product_dir: PathBuf,
    pub manifest: Manifest,
    _lock: Option<File>,
    _snapshot: Option<tempfile::TempDir>,
}

impl Drop for OpenedBuild {
    fn drop(&mut self) {
        if let Some(lock) = &self._lock {
            let _ = File::unlock(lock);
        }
    }
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
    sources: &'a [crate::manifest::SourceFile],
    inputs: &'a [crate::manifest::BuildInput],
    target: &'a crate::context::ResolvedTarget,
    observations: &'a [crate::manifest::Observation],
    modules: &'a [crate::manifest::ManifestModule],
    dependencies: &'a [crate::manifest::Dependency],
    artifacts: &'a [Artifact],
}

enum CurrentProduct {
    Missing,
    Valid(Box<Manifest>),
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
    let result = (|| {
        ensure_workspace_marker(&build_dir, &source_root)?;
        recover_publication(&build_dir)?;

        let host = options.host.map_or_else(HostContext::observe, Ok)?;
        let desired = match evaluate_with(
            &source_root,
            EvaluationOptions {
                project_arguments: options.project_arguments,
                host,
            },
        )? {
            EvaluationOutcome::Manifest(manifest) => *manifest,
            EvaluationOutcome::ProjectHelp(_) => {
                return Err(WombatError::configuration(
                    "project help was requested where a build was expected",
                ));
            }
        };
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
            return Ok(outcome(
                BuildStatus::Unchanged,
                build_dir.clone(),
                existing.as_ref().clone(),
            ));
        }
        let status = match current {
            CurrentProduct::Missing => BuildStatus::Created,
            CurrentProduct::Valid(_) => BuildStatus::Updated,
            CurrentProduct::Invalid => BuildStatus::Repaired,
        };

        publish(&build_dir, staging.path())?;
        let published = verify_product(&build_dir)?;
        Ok(outcome(status, build_dir.clone(), published))
    })();

    let unlock = File::unlock(&lock).map_err(|error| WombatError::io(&lock_path, error));
    match result {
        Err(error) => {
            let _ = unlock;
            Err(error)
        }
        Ok(outcome) => {
            unlock?;
            Ok(outcome)
        }
    }
}

pub fn project_help(source_root: &Path, host: Option<HostContext>) -> Result<String> {
    let source_root =
        fs::canonicalize(source_root).map_err(|error| WombatError::io(source_root, error))?;
    let host = host.map_or_else(HostContext::observe, Ok)?;
    match evaluate_with(
        &source_root,
        EvaluationOptions {
            project_arguments: vec![OsString::from("--help")],
            host,
        },
    )? {
        EvaluationOutcome::ProjectHelp(help) => Ok(help),
        EvaluationOutcome::Manifest(_) => Err(WombatError::configuration(
            "repository did not produce project help",
        )),
    }
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
    let result = verify_product(&build_dir);
    if let Some(lock) = &_lock {
        File::unlock(lock).map_err(|error| WombatError::io(&lock_path, error))?;
    }
    let manifest = result?;
    Ok(VerifiedBuild {
        build_dir,
        manifest,
    })
}

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
            acquire_shared(&lock, &requested_build_dir)?;
            let manifest = match verify_product(&requested_build_dir) {
                Ok(manifest) => manifest,
                Err(error) => {
                    let _ = File::unlock(&lock);
                    return Err(error);
                }
            };
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
    copy_product_directory(&source.join("tree"), &destination.join("tree"))
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
        && ["modules", "lua", "home", "dot_config", "dot_local"]
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
    materialise_with_hook(source_root, product_root, desired, |_| {})
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MaterialisationPoint {
    AfterArtifact(usize),
    BeforeFinalValidation,
}

fn materialise_with_hook(
    source_root: &Path,
    product_root: &Path,
    desired: crate::manifest::EvaluatedManifest,
    mut hook: impl FnMut(MaterialisationPoint),
) -> Result<Manifest> {
    let tree = product_root.join("tree");
    fs::create_dir(&tree).map_err(|error| WombatError::io(&tree, error))?;
    for anchor in ["home", "config"] {
        let path = tree.join(anchor);
        fs::create_dir(&path).map_err(|error| WombatError::io(&path, error))?;
    }

    let mut artifacts = Vec::with_capacity(desired.artifacts.len());
    for (index, artifact) in desired.artifacts.iter().enumerate() {
        artifacts.push(materialise_artifact(source_root, &tree, artifact)?);
        hook(MaterialisationPoint::AfterArtifact(index));
    }
    hook(MaterialisationPoint::BeforeFinalValidation);
    revalidate_sources(source_root, &desired.artifacts, &desired.directories)?;
    revalidate_lua_sources(source_root, &desired.sources)?;
    let mut manifest = Manifest {
        format_version: MANIFEST_FORMAT_VERSION,
        wombat_version: WOMBAT_VERSION.to_string(),
        build_id: String::new(),
        sources: desired.sources,
        inputs: desired.inputs,
        target: desired.target,
        observations: desired.observations,
        modules: desired.modules,
        dependencies: desired.dependencies,
        artifacts,
    };
    manifest.build_id = compute_build_id(&manifest)?;
    write_manifest(&product_root.join("manifest.json"), &manifest)?;
    Ok(manifest)
}

fn revalidate_lua_sources(
    source_root: &Path,
    sources: &[crate::manifest::SourceFile],
) -> Result<()> {
    for source in sources {
        let path = source_root.join(source.path.replace('/', std::path::MAIN_SEPARATOR_STR));
        reject_source_symlinks(source_root, &path)?;
        let metadata =
            fs::symlink_metadata(&path).map_err(|error| WombatError::io(&path, error))?;
        if !metadata.file_type().is_file() {
            return Err(WombatError::configuration(format!(
                "Lua source `{}` is no longer a regular file",
                source.path
            )));
        }
        let bytes = fs::read(&path).map_err(|error| WombatError::io(&path, error))?;
        let digest = digest_string(Sha256::digest(&bytes));
        if digest != source.digest {
            return Err(WombatError::configuration(format!(
                "Lua source `{}` changed during materialisation",
                source.path
            )));
        }
    }
    Ok(())
}

fn materialise_artifact(
    source_root: &Path,
    tree: &Path,
    artifact: &EvaluatedArtifact,
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
    let (production, content) = match &artifact.production {
        EvaluatedProduction::Static => (
            Production::Static,
            copy_and_hash(&source_path, &destination, &artifact.fingerprint)?,
        ),
        EvaluatedProduction::Template { context } => {
            let (source_digest, content) = render_and_hash(
                &source_path,
                &artifact.source,
                &destination,
                &artifact.fingerprint,
                context,
            )?;
            (
                Production::Template {
                    renderer: RendererIdentity {
                        name: TEMPLATE_RENDERER_NAME.to_string(),
                        contract_version: TEMPLATE_CONTRACT_VERSION,
                    },
                    source_digest,
                    context: context.clone(),
                },
                content,
            )
        }
    };
    Ok(Artifact {
        kind: artifact.kind,
        source: artifact.source.clone(),
        source_origin: artifact.source_origin.clone(),
        production,
        target: artifact.target.clone(),
        content,
        owner: artifact.owner.clone(),
        declared_at: artifact.declared_at.clone(),
    })
}

fn render_and_hash(
    source: &Path,
    source_name: &str,
    destination: &Path,
    expected: &SourceFingerprint,
    context: &crate::frozen::FrozenValue,
) -> Result<(String, FileContent)> {
    let mut input = File::open(source).map_err(|error| WombatError::io(source, error))?;
    let before = input
        .metadata()
        .map_err(|error| WombatError::io(source, error))?;
    if !before.file_type().is_file() || SourceFingerprint::from_metadata(&before) != *expected {
        return Err(source_changed(source));
    }
    let mut bytes = Vec::new();
    input
        .read_to_end(&mut bytes)
        .map_err(|error| WombatError::io(source, error))?;
    let after = input
        .metadata()
        .map_err(|error| WombatError::io(source, error))?;
    let path_after =
        fs::symlink_metadata(source).map_err(|error| WombatError::io(source, error))?;
    if SourceFingerprint::from_metadata(&after) != *expected
        || SourceFingerprint::from_metadata(&path_after) != *expected
    {
        return Err(source_changed(source));
    }
    let template_source = std::str::from_utf8(&bytes).map_err(|error| {
        WombatError::configuration(format!(
            "template source `{source_name}` is not valid UTF-8: {error}"
        ))
    })?;
    let source_digest = digest_string(Sha256::digest(&bytes));

    let mut renderer = handlebars::Handlebars::new();
    renderer.set_strict_mode(true);
    renderer.set_recursive_lookup(false);
    renderer.register_escape_fn(handlebars::no_escape);
    for helper in [
        "lookup", "log", "eq", "ne", "gt", "gte", "lt", "lte", "and", "or", "not", "len",
    ] {
        renderer.unregister_helper(helper);
    }
    renderer.register_helper("if", Box::new(StrictConditionalHelper::new("if", true)));
    renderer.register_helper(
        "unless",
        Box::new(StrictConditionalHelper::new("unless", false)),
    );
    let template = handlebars::Template::compile(template_source)
        .map_err(|error| template_compile_error(source_name, template_source, error))?;
    validate_handlebars_contract(source_name, &template)?;
    renderer.register_template(source_name, template);
    let rendered = renderer
        .render(source_name, context)
        .map_err(|error| template_render_error(source_name, template_source, error))?;
    let rendered = rendered.as_bytes();

    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)
        .map_err(|error| WombatError::io(destination, error))?;
    output
        .write_all(rendered)
        .map_err(|error| WombatError::io(destination, error))?;
    let executable = executable_intent(&before);
    set_normalized_permissions(&output, executable, destination)?;
    output
        .sync_all()
        .map_err(|error| WombatError::io(destination, error))?;
    Ok((
        source_digest,
        FileContent {
            digest: digest_string(Sha256::digest(rendered)),
            size: u64::try_from(rendered.len())
                .map_err(|_| WombatError::configuration("artifact size exceeds u64"))?,
            executable,
        },
    ))
}

fn template_compile_error(
    source_name: &str,
    source: &str,
    error: handlebars::TemplateError,
) -> WombatError {
    let position = error.pos();
    template_diagnostic(
        format!(
            "failed to compile template `{source_name}`: {}",
            error.reason()
        ),
        source_name,
        source,
        position,
        error.to_string(),
    )
}

fn template_render_error(
    source_name: &str,
    source: &str,
    error: handlebars::RenderError,
) -> WombatError {
    let position = error.line_no.zip(error.column_no);
    template_diagnostic(
        format!("failed to render template `{source_name}`: {error}"),
        source_name,
        source,
        position,
        error.to_string(),
    )
}

fn template_diagnostic(
    message: String,
    source_name: &str,
    source: &str,
    position: Option<(usize, usize)>,
    underlying: String,
) -> WombatError {
    let line = position.and_then(|(line, _)| u32::try_from(line).ok());
    let column = position.and_then(|(_, column)| u32::try_from(column).ok());
    let mut diagnostic = crate::Diagnostic::new(message);
    diagnostic.primary = Some(crate::manifest::SourceLocation {
        source: source_name.to_string(),
        line,
        column,
    });
    diagnostic.source_line = line.and_then(|line| {
        source
            .lines()
            .nth(line.saturating_sub(1) as usize)
            .map(str::to_string)
    });
    diagnostic.underlying = Some(underlying);
    WombatError::diagnostic(diagnostic)
}

struct StrictConditionalHelper {
    name: &'static str,
    positive: bool,
}

impl StrictConditionalHelper {
    fn new(name: &'static str, positive: bool) -> Self {
        Self { name, positive }
    }
}

impl handlebars::HelperDef for StrictConditionalHelper {
    fn call<'reg: 'rc, 'rc>(
        &self,
        helper: &handlebars::Helper<'rc>,
        renderer: &'reg handlebars::Handlebars<'reg>,
        context: &'rc handlebars::Context,
        render_context: &mut handlebars::RenderContext<'reg, 'rc>,
        output: &mut dyn handlebars::Output,
    ) -> handlebars::HelperResult {
        let value = helper
            .param(0)
            .ok_or(handlebars::RenderErrorReason::ParamNotFoundForIndex(
                self.name, 0,
            ))?;
        if value.is_value_missing() {
            return Err(handlebars::RenderError::strict_error(value.relative_path()));
        }
        let truthy = handlebars_truthy(value.value());
        let template = if truthy == self.positive {
            helper.template()
        } else {
            helper.inverse()
        };
        template.map_or(Ok(()), |template| {
            handlebars::Renderable::render(template, renderer, context, render_context, output)
        })
    }
}

fn handlebars_truthy(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(value) => *value,
        serde_json::Value::Number(value) => value.as_f64().is_some_and(f64::is_normal),
        serde_json::Value::String(value) => !value.is_empty(),
        serde_json::Value::Array(value) => !value.is_empty(),
        serde_json::Value::Object(value) => !value.is_empty(),
    }
}

fn validate_handlebars_contract(source_name: &str, template: &handlebars::Template) -> Result<()> {
    use handlebars::template::TemplateElement;

    for (index, element) in template.elements.iter().enumerate() {
        let location = template
            .mapping
            .get(index)
            .map_or(String::new(), |mapping| {
                format!(" at line {}, column {}", mapping.0, mapping.1)
            });
        match element {
            TemplateElement::RawString(_) | TemplateElement::Comment(_) => {}
            TemplateElement::Expression(helper) | TemplateElement::HtmlExpression(helper) => {
                if !helper.params.is_empty() || !helper.hash.is_empty() {
                    return Err(unsupported_handlebars_feature(
                        source_name,
                        &location,
                        "inline helpers",
                    ));
                }
            }
            TemplateElement::HelperBlock(helper) => {
                let name = helper.name.as_name().unwrap_or("<dynamic>");
                if !matches!(name, "if" | "unless" | "each" | "with" | "raw") {
                    return Err(unsupported_handlebars_feature(
                        source_name,
                        &location,
                        &format!("helper `{name}`"),
                    ));
                }
                if !helper.hash.is_empty()
                    || helper
                        .params
                        .iter()
                        .any(handlebars_parameter_has_subexpression)
                {
                    return Err(unsupported_handlebars_feature(
                        source_name,
                        &location,
                        "helper hash arguments and subexpressions",
                    ));
                }
                if matches!(name, "each" | "with") && helper.inverse.is_some() {
                    return Err(unsupported_handlebars_feature(
                        source_name,
                        &location,
                        "else blocks on `each` or `with`",
                    ));
                }
                if let Some(body) = &helper.template {
                    validate_handlebars_contract(source_name, body)?;
                }
                if let Some(inverse) = &helper.inverse {
                    validate_handlebars_contract(source_name, inverse)?;
                }
            }
            TemplateElement::DecoratorExpression(_)
            | TemplateElement::DecoratorBlock(_)
            | TemplateElement::PartialExpression(_)
            | TemplateElement::PartialBlock(_) => {
                return Err(unsupported_handlebars_feature(
                    source_name,
                    &location,
                    "decorators and partials",
                ));
            }
            _ => {
                return Err(unsupported_handlebars_feature(
                    source_name,
                    &location,
                    "this template construct",
                ));
            }
        }
    }
    Ok(())
}

fn handlebars_parameter_has_subexpression(parameter: &handlebars::template::Parameter) -> bool {
    matches!(parameter, handlebars::template::Parameter::Subexpression(_))
}

fn unsupported_handlebars_feature(source_name: &str, location: &str, feature: &str) -> WombatError {
    WombatError::configuration(format!(
        "template `{source_name}` uses unsupported Handlebars {feature}{location}; resolve policy and transformations in Lua"
    ))
}

fn copy_and_hash(
    source: &Path,
    destination: &Path,
    expected: &SourceFingerprint,
) -> Result<FileContent> {
    copy_and_hash_with_hook(source, destination, expected, || {})
}

fn copy_and_hash_with_hook(
    source: &Path,
    destination: &Path,
    expected: &SourceFingerprint,
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
    if SourceFingerprint::from_metadata(&before) != *expected {
        return Err(source_changed(source));
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
    if SourceFingerprint::from_metadata(&after) != *expected
        || SourceFingerprint::from_metadata(&path_after) != *expected
    {
        return Err(source_changed(source));
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

fn source_changed(source: &Path) -> WombatError {
    WombatError::configuration(format!(
        "artifact source `{}` changed during materialisation",
        source.display()
    ))
}

fn revalidate_sources(
    source_root: &Path,
    artifacts: &[EvaluatedArtifact],
    directories: &[EvaluatedDirectory],
) -> Result<()> {
    for artifact in artifacts {
        let source = source_root.join(&artifact.source);
        if fingerprint_regular_file(&source)? != artifact.fingerprint {
            return Err(source_changed(&source));
        }
    }
    for directory in directories {
        let source = source_root.join(&directory.root);
        if snapshot_directory(source_root, &source)? != directory.snapshot {
            return Err(WombatError::configuration(format!(
                "static directory source `{}` changed during materialisation",
                source.display()
            )));
        }
    }
    Ok(())
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
        sources: &manifest.sources,
        inputs: &manifest.inputs,
        target: &manifest.target,
        observations: &manifest.observations,
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

pub(crate) fn validate_manifest(manifest: &Manifest) -> Result<()> {
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
        .sources
        .windows(2)
        .all(|pair| pair[0].path < pair[1].path)
    {
        return Err(WombatError::configuration(
            "manifest Lua sources are not uniquely sorted",
        ));
    }
    let source_paths = manifest
        .sources
        .iter()
        .map(|source| source.path.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    for source in &manifest.sources {
        validate_relative_path(&source.path, "manifest Lua source path")?;
        validate_sha256(&source.digest, "manifest Lua source digest")?;
    }
    crate::context::TargetPlatform::from_frozen(&manifest.target.platform.to_frozen())?;
    match (&manifest.target.origin, &manifest.target.declared_at) {
        (crate::context::TargetOrigin::HostDefault, None)
        | (crate::context::TargetOrigin::RootOverride, Some(_)) => {}
        _ => {
            return Err(WombatError::configuration(
                "manifest target origin and declaration location are inconsistent",
            ));
        }
    }
    if let Some(trace) = &manifest.target.declared_at {
        validate_source_trace(trace, &source_paths, "manifest target declaration")?;
    }
    if !manifest
        .inputs
        .windows(2)
        .all(|pair| pair[0].name < pair[1].name)
    {
        return Err(WombatError::configuration(
            "manifest build inputs are not uniquely sorted",
        ));
    }
    for input in &manifest.inputs {
        let mut name = input.name.bytes();
        if !name
            .next()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
            || !name.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return Err(WombatError::configuration(format!(
                "manifest build input name `{}` is invalid",
                input.name
            )));
        }
        validate_source_trace(
            &input.declared_at,
            &source_paths,
            "manifest input declaration",
        )?;
        match input.kind {
            crate::manifest::BuildInputKind::Flag
                if !matches!(input.value, crate::frozen::FrozenValue::Boolean(_)) =>
            {
                return Err(WombatError::configuration(
                    "manifest flag input is not boolean",
                ));
            }
            crate::manifest::BuildInputKind::Choice
            | crate::manifest::BuildInputKind::String
            | crate::manifest::BuildInputKind::Target
                if !matches!(input.value, crate::frozen::FrozenValue::String(_)) =>
            {
                return Err(WombatError::configuration(
                    "manifest textual input is not a string",
                ));
            }
            crate::manifest::BuildInputKind::Integer
                if !matches!(input.value, crate::frozen::FrozenValue::Integer(_)) =>
            {
                return Err(WombatError::configuration(
                    "manifest integer input is not an integer",
                ));
            }
            _ => {}
        }
        if input.kind == crate::manifest::BuildInputKind::Target
            && let crate::frozen::FrozenValue::String(value) = &input.value
        {
            let parsed = crate::context::TargetPlatform::parse_compact(value)?;
            if parsed.compact() != *value {
                return Err(WombatError::configuration(
                    "manifest target input is not canonical",
                ));
            }
        }
    }
    if !manifest.observations.windows(2).all(|pair| {
        (pair[0].subject, pair[0].path.as_str()) < (pair[1].subject, pair[1].path.as_str())
    }) {
        return Err(WombatError::configuration(
            "manifest observations are not uniquely sorted",
        ));
    }
    if manifest.observations.iter().any(|observation| {
        observation.path.is_empty()
            || observation.path.split('.').any(|component| {
                component.is_empty()
                    || !component
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            })
    }) {
        return Err(WombatError::configuration(
            "manifest contains an invalid observation path",
        ));
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
    for module in &manifest.modules {
        validate_relative_path(&module.source, "manifest module source")?;
        if !source_paths.contains(module.source.as_str()) {
            return Err(WombatError::configuration(format!(
                "manifest module `{}` references uncatalogued source `{}`",
                module.name, module.source
            )));
        }
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
    for dependency in &manifest.dependencies {
        validate_source_trace(
            &dependency.declared_at,
            &source_paths,
            "manifest dependency declaration",
        )?;
    }
    if !manifest.artifacts.windows(2).all(|pair| {
        pair[0]
            .target
            .key()
            .cmp(&pair[1].target.key())
            .then_with(|| pair[0].owner.cmp(&pair[1].owner))
            .then_with(|| pair[0].source.cmp(&pair[1].source))
            .then_with(|| pair[0].declared_at.cmp(&pair[1].declared_at))
            .is_lt()
    }) {
        return Err(WombatError::configuration(
            "manifest artifacts are not uniquely sorted",
        ));
    }
    for artifact in &manifest.artifacts {
        validate_relative_path(&artifact.source, "manifest artifact source")?;
        validate_source_trace(
            &artifact.declared_at,
            &source_paths,
            "manifest artifact declaration",
        )?;
        validate_relative_path(&artifact.target.path, "manifest target path")?;
        match &artifact.source_origin {
            SourceOrigin::Direct { declared } => {
                validate_declared_source(declared)?;
                if declared == "." {
                    return Err(WombatError::configuration(
                        "manifest direct artifact source must identify a file",
                    ));
                }
                let expected_source = resolve_declared_manifest_source(
                    declared,
                    &artifact.declared_at.primary.source,
                );
                if artifact.source != expected_source {
                    return Err(WombatError::configuration(format!(
                        "manifest direct source `{}` does not match declared source `{expected_source}`",
                        artifact.source
                    )));
                }
            }
            SourceOrigin::Directory {
                declared,
                root,
                relative,
            } => {
                validate_declared_source(declared)?;
                validate_relative_path(root, "manifest directory source root")?;
                validate_relative_path(relative, "manifest directory relative path")?;
                let expected_root = resolve_declared_manifest_source(
                    declared,
                    &artifact.declared_at.primary.source,
                );
                if root != &expected_root {
                    return Err(WombatError::configuration(format!(
                        "manifest directory root `{root}` does not match declared root `{expected_root}`"
                    )));
                }
                let expected_source = format!("{root}/{relative}");
                if artifact.source != expected_source {
                    return Err(WombatError::configuration(format!(
                        "manifest directory source `{}` does not match `{expected_source}`",
                        artifact.source
                    )));
                }
                if !matches!(artifact.production, Production::Static) {
                    return Err(WombatError::configuration(
                        "manifest directory-expanded artifacts must use static production",
                    ));
                }
            }
        }
        match &artifact.production {
            Production::Static => {}
            Production::Template {
                renderer,
                source_digest,
                context,
            } => {
                if renderer.name != TEMPLATE_RENDERER_NAME
                    || renderer.contract_version != TEMPLATE_CONTRACT_VERSION
                {
                    return Err(WombatError::configuration(format!(
                        "unsupported template renderer contract `{}-v{}`",
                        renderer.name, renderer.contract_version
                    )));
                }
                if source_digest.len() != 71
                    || !source_digest.starts_with("sha256:")
                    || !source_digest[7..]
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit())
                {
                    return Err(WombatError::configuration(
                        "manifest template source digest is not a SHA-256 identity",
                    ));
                }
                if !matches!(context, crate::frozen::FrozenValue::Map(_)) {
                    return Err(WombatError::configuration(
                        "manifest template context must be a map",
                    ));
                }
            }
        }
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
            TargetOrigin::Inferred {
                basis,
                source_anchor,
            } => {
                let expected = manifest_declaration_anchor(&artifact.declared_at.primary.source)
                    .map_or_else(
                        || {
                            crate::manifest::SourceAnchor::ALL
                                .into_iter()
                                .find(|anchor| {
                                    artifact.source == anchor.source_prefix()
                                        || artifact
                                            .source
                                            .starts_with(&format!("{}/", anchor.source_prefix()))
                                })
                                .map(|anchor| {
                                    (crate::manifest::InferenceBasis::SourcePrefix, anchor)
                                })
                        },
                        |anchor| Some((crate::manifest::InferenceBasis::ModuleAnchor, anchor)),
                    );
                if expected != Some((*basis, *source_anchor)) {
                    return Err(WombatError::configuration(format!(
                        "manifest inferred target `{}` has inconsistent inference provenance",
                        artifact.target.display
                    )));
                }
                let prefix = format!("{}/", source_anchor.source_prefix());
                let relative = artifact.source.strip_prefix(&prefix).ok_or_else(|| {
                    WombatError::configuration(format!(
                        "manifest inferred source `{}` is outside its {:?} source anchor",
                        artifact.source, source_anchor
                    ))
                })?;
                let relative = if matches!(artifact.production, Production::Template { .. }) {
                    relative.strip_suffix(".tmpl").unwrap_or(relative)
                } else {
                    relative
                };
                let parsed = infer_target(*source_anchor, relative, *basis)?;
                if parsed.anchor != artifact.target.anchor
                    || parsed.path != artifact.target.path
                    || parsed.display != artifact.target.display
                {
                    return Err(WombatError::configuration(format!(
                        "manifest inferred target `{}` does not match its concrete source",
                        artifact.target.display
                    )));
                }
            }
            TargetOrigin::DirectoryExplicit { declared, relative } => {
                let root = parse_explicit_target_root(declared)?;
                let parsed = expand_target_root(&root, relative)?;
                if parsed.anchor != artifact.target.anchor
                    || parsed.path != artifact.target.path
                    || parsed.display != artifact.target.display
                {
                    return Err(WombatError::configuration(format!(
                        "manifest directory target `{declared}` plus `{relative}` does not match its resolved target"
                    )));
                }
            }
        }
    }
    Ok(())
}

fn validate_sha256(value: &str, label: &str) -> Result<()> {
    if value.len() != 71
        || !value.starts_with("sha256:")
        || !value[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(WombatError::configuration(format!(
            "{label} is not a SHA-256 identity"
        )));
    }
    Ok(())
}

fn validate_source_trace(
    trace: &crate::manifest::SourceTrace,
    sources: &std::collections::BTreeSet<&str>,
    label: &str,
) -> Result<()> {
    if trace.callers.len() + 1 > crate::manifest::MAX_SOURCE_TRACE_FRAMES {
        return Err(WombatError::configuration(format!(
            "{label} exceeds the maximum source trace depth"
        )));
    }
    let mut previous = None;
    for location in std::iter::once(&trace.primary).chain(&trace.callers) {
        validate_relative_path(&location.source, &format!("{label} source"))?;
        if !sources.contains(location.source.as_str()) {
            return Err(WombatError::configuration(format!(
                "{label} references uncatalogued source `{}`",
                location.source
            )));
        }
        if location.line == Some(0) || location.column == Some(0) {
            return Err(WombatError::configuration(format!(
                "{label} contains a zero source position"
            )));
        }
        if location.column.is_some() && location.line.is_none() {
            return Err(WombatError::configuration(format!(
                "{label} contains a column without a line"
            )));
        }
        if previous == Some(location) {
            return Err(WombatError::configuration(format!(
                "{label} contains consecutive duplicate frames"
            )));
        }
        previous = Some(location);
    }
    Ok(())
}

fn resolve_declared_manifest_source(declared: &str, declared_from: &str) -> String {
    let prefix = manifest_declaration_anchor(declared_from)
        .map(crate::manifest::SourceAnchor::source_prefix);
    match (prefix, declared) {
        (Some(prefix), ".") => prefix.to_string(),
        (Some(prefix), _) => format!("{prefix}/{declared}"),
        (None, ".") => ".".to_string(),
        (None, _) => declared.to_string(),
    }
}

fn manifest_declaration_anchor(declared_from: &str) -> Option<crate::manifest::SourceAnchor> {
    if declared_from.starts_with("modules/dot_config/") {
        Some(crate::manifest::SourceAnchor::DotConfig)
    } else if declared_from.starts_with("modules/dot_local/") {
        Some(crate::manifest::SourceAnchor::DotLocal)
    } else if declared_from.starts_with("modules/home/") {
        Some(crate::manifest::SourceAnchor::Home)
    } else {
        None
    }
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
            Ok(manifest) => CurrentProduct::Valid(Box::new(manifest)),
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
    use crate::runtime::evaluate;

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
        let expected = fingerprint_regular_file(&source).unwrap();

        let error = copy_and_hash_with_hook(&source, &destination, &expected, || {
            fs::write(&source, "changed while materialising\n").unwrap();
        })
        .unwrap_err()
        .to_string();

        assert!(error.contains("changed during materialisation"), "{error}");
    }

    #[test]
    fn template_source_mutation_before_final_validation_is_rejected() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("repository");
        let staged = temporary.path().join("staged");
        fs::create_dir_all(source.join("modules/dot_config")).unwrap();
        fs::create_dir_all(source.join("dot_config")).unwrap();
        fs::create_dir(&staged).unwrap();
        fs::write(
            source.join("wombat.lua"),
            "local w = require('wombat')\nw.use('app')\n",
        )
        .unwrap();
        fs::write(
            source.join("modules/dot_config/app.lua"),
            "local w = require('wombat')\nw.install('app.tmpl', { with = { value = 'before' } })\n",
        )
        .unwrap();
        let template = source.join("dot_config/app.tmpl");
        fs::write(&template, "{{ value }}\n").unwrap();
        let desired = evaluate(&source).unwrap();

        let error = materialise_with_hook(&source, &staged, desired, |point| {
            if point == MaterialisationPoint::BeforeFinalValidation {
                fs::write(&template, "changed {{ value }}\n").unwrap();
            }
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("changed during materialisation"), "{error}");
    }

    #[test]
    fn lua_source_mutation_before_final_validation_is_rejected() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("repository");
        let staged = temporary.path().join("staged");
        repository(&source);
        fs::create_dir(&staged).unwrap();
        let desired = evaluate(&source).unwrap();
        let module = source.join("modules/dot_config/app.lua");

        let error = materialise_with_hook(&source, &staged, desired, |point| {
            if point == MaterialisationPoint::BeforeFinalValidation {
                fs::write(
                    &module,
                    "-- changed\nlocal w = require(\"wombat\")\nw.install(\"app.toml\")\n",
                )
                .unwrap();
            }
        })
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("Lua source") && error.contains("changed"),
            "{error}"
        );
    }

    #[test]
    fn final_directory_rewalk_rejects_every_membership_and_metadata_change() {
        #[derive(Clone, Copy, Debug)]
        enum Mutation {
            Content,
            Add,
            Remove,
            Rename,
            Type,
            #[cfg(unix)]
            Mode,
        }

        let mutations = [
            Mutation::Content,
            Mutation::Add,
            Mutation::Remove,
            Mutation::Rename,
            Mutation::Type,
            #[cfg(unix)]
            Mutation::Mode,
        ];
        for mutation in mutations {
            let temporary = tempfile::tempdir().unwrap();
            let source = temporary.path().join("repository");
            let current = temporary.path().join("current");
            fs::create_dir_all(source.join("modules/dot_config")).unwrap();
            fs::create_dir_all(source.join("dot_config/tree")).unwrap();
            fs::write(
                source.join("wombat.lua"),
                "local w = require(\"wombat\")\nw.use(\"tree\")\n",
            )
            .unwrap();
            fs::write(
                source.join("modules/dot_config/tree.lua"),
                "local w = require(\"wombat\")\nw.install(\"tree\")\n",
            )
            .unwrap();
            let leaf = source.join("dot_config/tree/file");
            fs::write(&leaf, "before\n").unwrap();
            let previous = build(BuildOptions::new(&source, &current)).unwrap();
            let desired = evaluate(&source).unwrap();
            let staged = temporary.path().join("staged");
            fs::create_dir(&staged).unwrap();

            let error = materialise_with_hook(&source, &staged, desired, |point| {
                if point != MaterialisationPoint::BeforeFinalValidation {
                    return;
                }
                match mutation {
                    Mutation::Content => fs::write(&leaf, "changed\n").unwrap(),
                    Mutation::Add => {
                        fs::write(source.join("dot_config/tree/added"), "added\n").unwrap();
                    }
                    Mutation::Remove => fs::remove_file(&leaf).unwrap(),
                    Mutation::Rename => {
                        fs::rename(&leaf, source.join("dot_config/tree/renamed")).unwrap();
                    }
                    Mutation::Type => {
                        fs::remove_file(&leaf).unwrap();
                        fs::create_dir(&leaf).unwrap();
                    }
                    #[cfg(unix)]
                    Mutation::Mode => {
                        use std::os::unix::fs::PermissionsExt;
                        fs::set_permissions(&leaf, fs::Permissions::from_mode(0o755)).unwrap();
                    }
                }
            })
            .unwrap_err()
            .to_string();

            assert!(
                error.contains("changed during materialisation")
                    || error.contains("No such file")
                    || error.contains("not a regular file"),
                "{mutation:?}: {error}"
            );
            let verified = verify_build(&current).unwrap();
            assert_eq!(verified.manifest.build_id, previous.build_id);
        }
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
