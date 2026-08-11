use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use tempfile::NamedTempFile;

use crate::manifest::{EvaluatedDirectory, InferenceBasis, SourceOrigin, TargetPath};
use crate::path::{
    expand_target_root, infer_target, prefixed_source, reject_noncanonical_artifact_trees,
    validate_relative_path,
};
use crate::runtime::evaluate;
use crate::{Result, WombatError};

const AUTO_MODULE: &str = "modules/auto.lua";
const BEGIN_SENTINEL: &str = "-- wombat:add begin";
const END_SENTINEL: &str = "-- wombat:add end";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AddStatus {
    Added,
    DeclarationAdded,
    AlreadyPresent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AddOutcome {
    pub status: AddStatus,
    pub source: String,
    pub method: AddMethod,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AddMethod {
    GeneratedAuto,
    Directory {
        owner: String,
        declared_source: String,
    },
}

impl AddOutcome {
    pub fn display(&self) -> String {
        let action = match self.status {
            AddStatus::Added => "added",
            AddStatus::DeclarationAdded => "declared existing source",
            AddStatus::AlreadyPresent => "already added",
        };
        match &self.method {
            AddMethod::GeneratedAuto => {
                format!("{action} `{}` through module `auto`", self.source)
            }
            AddMethod::Directory {
                owner,
                declared_source,
            } => format!(
                "{action} `{}` through directory `{declared_source}` owned by module `{owner}`",
                self.source
            ),
        }
    }
}

impl fmt::Display for AddOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.display())
    }
}

pub fn add(root: &Path, target_home: &Path, target: &Path) -> Result<AddOutcome> {
    if !target.is_absolute() {
        return Err(WombatError::configuration(format!(
            "add target `{}` must be an absolute path",
            target.display()
        )));
    }

    let root = fs::canonicalize(root).map_err(|error| WombatError::io(root, error))?;
    reject_noncanonical_artifact_trees(&root)?;
    let home =
        fs::canonicalize(target_home).map_err(|error| WombatError::io(target_home, error))?;

    let target_metadata =
        fs::symlink_metadata(target).map_err(|error| WombatError::io(target, error))?;
    if target_metadata.file_type().is_symlink() {
        return Err(WombatError::configuration(format!(
            "add target `{}` must not be a symbolic link",
            target.display()
        )));
    }
    if target_metadata.file_type().is_dir() {
        return add_directory(&root, &home, target);
    }
    if !target_metadata.file_type().is_file() {
        return Err(WombatError::configuration(format!(
            "add target `{}` must be a regular file",
            target.display()
        )));
    }
    let target_executable = is_executable(&target_metadata);
    let target = fs::canonicalize(target).map_err(|error| WombatError::io(target, error))?;
    let home_relative = target.strip_prefix(&home).map_err(|_| {
        WombatError::configuration(format!(
            "add target `{}` must resolve beneath target home `{}`",
            target.display(),
            home.display()
        ))
    })?;
    let source = source_path_for_home_file(home_relative)?;
    validate_relative_path(&source, "generated artifact source")?;
    let source_path = root.join(source.replace('/', std::path::MAIN_SEPARATOR_STR));
    validate_destination_path(&root, &source_path)?;

    let manifest = evaluate(&root)?;
    let (source_anchor, target_relative) = prefixed_source(&source)?
        .expect("generated add sources always contain a recognized anchor prefix");
    let prospective_target =
        infer_target(source_anchor, target_relative, InferenceBasis::SourcePrefix)?;
    let coverages = directory_coverages(&manifest.directories, &source)?;
    validate_prospective_outputs(
        &target,
        &source,
        &manifest.artifacts,
        &coverages,
        &prospective_target,
    )?;

    let target_bytes = fs::read(&target).map_err(|error| WombatError::io(&target, error))?;
    let source_exists = source_path
        .try_exists()
        .map_err(|error| WombatError::io(&source_path, error))?;
    if source_exists {
        let metadata = fs::symlink_metadata(&source_path)
            .map_err(|error| WombatError::io(&source_path, error))?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            return Err(WombatError::configuration(format!(
                "source state `{source}` must be a regular non-symlink file"
            )));
        }
        let existing =
            fs::read(&source_path).map_err(|error| WombatError::io(&source_path, error))?;
        if existing != target_bytes || is_executable(&metadata) != target_executable {
            return Err(WombatError::configuration(format!(
                "source state `{source}` already exists with different contents or executable intent; overwrite and re-add are not supported in this slice"
            )));
        }
    }

    let matching = coverages
        .iter()
        .filter(|coverage| coverage.target.key() == prospective_target.key())
        .collect::<Vec<_>>();
    if matching.len() > 1 {
        let declarations = matching
            .iter()
            .map(|coverage| {
                format!(
                    "`{}` owned by `{}` at {}",
                    coverage.directory.declared_source,
                    coverage.directory.owner,
                    coverage.directory.declared_at
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        return Err(WombatError::configuration(format!(
            "cannot add `{}` because multiple directory declarations map `{source}` to `{}`: {declarations}",
            target.display(),
            prospective_target.display
        )));
    }
    if let Some(coverage) = matching.first() {
        let method = AddMethod::Directory {
            owner: coverage.directory.owner.clone(),
            declared_source: coverage.directory.declared_source.clone(),
        };
        if source_exists {
            return Ok(AddOutcome {
                status: AddStatus::AlreadyPresent,
                source,
                method,
            });
        }
        persist_addition(
            &root,
            &source_path,
            Some((target_bytes.as_slice(), target_executable)),
            None,
        )?;
        return Ok(AddOutcome {
            status: AddStatus::Added,
            source,
            method,
        });
    }

    let auto_path = root.join(AUTO_MODULE);
    let auto_metadata = fs::symlink_metadata(&auto_path).map_err(|_| {
        WombatError::configuration(format!(
            "`{AUTO_MODULE}` is required before `wombat add`; create the standard generated module and select it with `w.use(\"auto\")`"
        ))
    })?;
    if auto_metadata.file_type().is_symlink() || !auto_metadata.file_type().is_file() {
        return Err(WombatError::configuration(format!(
            "`{AUTO_MODULE}` must be a regular non-symlink file"
        )));
    }
    let auto_source =
        fs::read_to_string(&auto_path).map_err(|error| WombatError::io(&auto_path, error))?;
    let mut generated = parse_generated_region(&auto_source).map_err(|message| {
        WombatError::configuration(format!(
            "cannot update `{AUTO_MODULE}`: {message}; proposed declaration: {}",
            generated_line(&source)
        ))
    })?;
    if !manifest.modules.iter().any(|module| module.name == "auto") {
        return Err(WombatError::configuration(
            "module `auto` is not selected; add `w.use(\"auto\")` to root policy before using `wombat add`",
        ));
    }
    let declaration_exists = generated.contains(&source);
    for artifact in &manifest.artifacts {
        if targets_overlap(&artifact.target, &prospective_target)
            && !(declaration_exists
                && artifact.owner == "auto"
                && artifact.source == source
                && artifact.target.key() == prospective_target.key())
        {
            return Err(WombatError::configuration(format!(
                "cannot add `{}` because target `{}` overlaps an artifact owned by `{}` from `{}` declared at {}",
                target.display(),
                prospective_target.display,
                artifact.owner,
                artifact.source,
                artifact.declared_at
            )));
        }
    }

    let declaration_added = generated.insert(source.clone());
    if source_exists && !declaration_added {
        return Ok(AddOutcome {
            status: AddStatus::AlreadyPresent,
            source,
            method: AddMethod::GeneratedAuto,
        });
    }

    let updated_auto = render_generated_region(&auto_source, &generated)
        .expect("a parsed generated region can always be rendered");
    persist_addition(
        &root,
        &source_path,
        (!source_exists).then_some((target_bytes.as_slice(), target_executable)),
        Some((
            &auto_path,
            declaration_added.then_some(updated_auto.as_bytes()),
            &auto_metadata,
        )),
    )?;

    Ok(AddOutcome {
        status: if source_exists {
            AddStatus::DeclarationAdded
        } else {
            AddStatus::Added
        },
        source,
        method: AddMethod::GeneratedAuto,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ImportedLeaf {
    relative: String,
    bytes: Vec<u8>,
    fingerprint: crate::source::SourceFingerprint,
    executable: bool,
}

fn add_directory(root: &Path, home: &Path, requested: &Path) -> Result<AddOutcome> {
    let target = fs::canonicalize(requested).map_err(|error| WombatError::io(requested, error))?;
    validate_target_components(home, &target)?;
    let relative = target.strip_prefix(home).map_err(|_| {
        WombatError::configuration(format!(
            "add target `{}` must resolve beneath target home `{}`",
            target.display(),
            home.display()
        ))
    })?;
    let source = source_path_for_home_directory(relative)?;
    validate_relative_path(&source, "generated directory source")?;
    let source_path = root.join(source.replace('/', std::path::MAIN_SEPARATOR_STR));
    validate_destination_path(root, &source_path)?;
    let target_leaves = snapshot_import_tree(&target)?;
    if target_leaves.is_empty() {
        return Err(WombatError::configuration(format!(
            "add target directory `{}` contains no regular files",
            target.display()
        )));
    }

    let manifest = evaluate(root)?;
    let mut coverage_identity: Option<(String, String)> = None;
    let mut any_coverage = false;
    for leaf in &target_leaves {
        let leaf_source = format!("{source}/{}", leaf.relative);
        let (anchor, target_relative) = prefixed_source(&leaf_source)?
            .expect("generated directory sources use canonical anchors");
        let expected = infer_target(anchor, target_relative, InferenceBasis::SourcePrefix)?;
        let coverages = directory_coverages(&manifest.directories, &leaf_source)?;
        validate_prospective_outputs(
            &target.join(portable_path(&leaf.relative)),
            &leaf_source,
            &manifest.artifacts,
            &coverages,
            &expected,
        )?;
        if coverages.len() > 1 {
            return Err(WombatError::configuration(format!(
                "cannot add directory `{}` because `{leaf_source}` has ambiguous directory ownership",
                target.display()
            )));
        }
        if let Some(coverage) = coverages.first() {
            any_coverage = true;
            if coverage.target.key() != expected.key() {
                return Err(WombatError::configuration(format!(
                    "cannot add directory `{}` because existing directory `{}` maps `{leaf_source}` to `{}` instead of `{}`",
                    target.display(),
                    coverage.directory.declared_source,
                    coverage.target.display,
                    expected.display
                )));
            }
            let identity = (
                coverage.directory.owner.clone(),
                coverage.directory.declared_source.clone(),
            );
            if coverage_identity
                .as_ref()
                .is_some_and(|prior| prior != &identity)
            {
                return Err(WombatError::configuration(format!(
                    "cannot add directory `{}` because its leaves have different existing owners",
                    target.display()
                )));
            }
            coverage_identity = Some(identity);
        } else if any_coverage || coverage_identity.is_some() {
            return Err(WombatError::configuration(format!(
                "cannot add directory `{}` because only part of the tree has existing directory coverage",
                target.display()
            )));
        }
    }
    if any_coverage
        && target_leaves.iter().any(|leaf| {
            let leaf_source = format!("{source}/{}", leaf.relative);
            directory_coverages(&manifest.directories, &leaf_source)
                .map_or(true, |coverages| coverages.is_empty())
        })
    {
        return Err(WombatError::configuration(format!(
            "cannot add directory `{}` because only part of the tree has existing directory coverage",
            target.display()
        )));
    }
    if !any_coverage {
        for leaf in &target_leaves {
            let leaf_source = format!("{source}/{}", leaf.relative);
            let (anchor, target_relative) = prefixed_source(&leaf_source)?
                .expect("generated directory sources use canonical anchors");
            let prospective = infer_target(anchor, target_relative, InferenceBasis::SourcePrefix)?;
            if let Some(artifact) = manifest
                .artifacts
                .iter()
                .find(|artifact| targets_overlap(&artifact.target, &prospective))
            {
                return Err(WombatError::configuration(format!(
                    "cannot add directory `{}` because target `{}` overlaps an artifact owned by `{}` from `{}` declared at {}",
                    target.display(),
                    prospective.display,
                    artifact.owner,
                    artifact.source,
                    artifact.declared_at
                )));
            }
        }
    }

    let existing_leaves = match fs::symlink_metadata(&source_path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() => {
            return Err(WombatError::configuration(format!(
                "source state `{source}` must be a non-symlink directory"
            )));
        }
        Ok(_) => Some(snapshot_import_tree(&source_path)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(WombatError::io(&source_path, error)),
    };
    if let Some(existing) = &existing_leaves
        && existing.iter().any(|left| {
            target_leaves
                .iter()
                .find(|right| right.relative == left.relative)
                .is_none_or(|right| {
                    left.bytes != right.bytes || left.executable != right.executable
                })
        })
    {
        return Err(WombatError::configuration(format!(
            "source state `{source}` already exists with a different directory tree"
        )));
    }
    let source_complete = existing_leaves
        .as_ref()
        .is_some_and(|existing| existing.len() == target_leaves.len());

    let (method, auto_update) = if let Some((owner, declared_source)) = coverage_identity {
        (
            AddMethod::Directory {
                owner,
                declared_source,
            },
            None,
        )
    } else {
        let auto_path = root.join(AUTO_MODULE);
        let auto_metadata = fs::symlink_metadata(&auto_path).map_err(|_| {
            WombatError::configuration(format!(
                "`{AUTO_MODULE}` is required before `wombat add`; run `wombat init` or create and select the standard generated module"
            ))
        })?;
        if auto_metadata.file_type().is_symlink() || !auto_metadata.file_type().is_file() {
            return Err(WombatError::configuration(format!(
                "`{AUTO_MODULE}` must be a regular non-symlink file"
            )));
        }
        if !manifest.modules.iter().any(|module| module.name == "auto") {
            return Err(WombatError::configuration(
                "module `auto` is not selected; add `w.use(\"auto\")` to root policy before using `wombat add`",
            ));
        }
        let auto_source =
            fs::read_to_string(&auto_path).map_err(|error| WombatError::io(&auto_path, error))?;
        let mut generated = parse_generated_region(&auto_source).map_err(|message| {
            WombatError::configuration(format!(
                "cannot update `{AUTO_MODULE}`: {message}; proposed declaration: {}",
                generated_line(&source)
            ))
        })?;
        let declaration_added = generated.insert(source.clone());
        let updated = render_generated_region(&auto_source, &generated)
            .expect("a parsed generated region can always be rendered");
        (
            AddMethod::GeneratedAuto,
            Some((
                auto_path,
                auto_source,
                declaration_added.then_some(updated),
                crate::source::SourceFingerprint::from_metadata(&auto_metadata),
            )),
        )
    };

    let declaration_added = auto_update
        .as_ref()
        .is_some_and(|(_, _, updated, _)| updated.is_some());
    if source_complete && !declaration_added {
        return Ok(AddOutcome {
            status: AddStatus::AlreadyPresent,
            source,
            method,
        });
    }
    persist_directory_addition(
        root,
        &source_path,
        &target,
        &target_leaves,
        existing_leaves.as_deref(),
        auto_update.as_ref().map(|(path, old, new, fingerprint)| {
            (path.as_path(), old.as_str(), new.as_deref(), fingerprint)
        }),
    )?;
    Ok(AddOutcome {
        status: if source_complete {
            AddStatus::DeclarationAdded
        } else {
            AddStatus::Added
        },
        source,
        method,
    })
}

fn snapshot_import_tree(root: &Path) -> Result<Vec<ImportedLeaf>> {
    fn walk(root: &Path, directory: &Path, leaves: &mut Vec<ImportedLeaf>) -> Result<()> {
        let mut entries = fs::read_dir(directory)
            .map_err(|error| WombatError::io(directory, error))?
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(|error| WombatError::io(directory, error))?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let metadata =
                fs::symlink_metadata(&path).map_err(|error| WombatError::io(&path, error))?;
            if metadata.file_type().is_symlink() {
                return Err(WombatError::configuration(format!(
                    "add directory entry `{}` must not be a symbolic link",
                    path.display()
                )));
            }
            if metadata.file_type().is_dir() {
                walk(root, &path, leaves)?;
            } else if metadata.file_type().is_file() {
                let relative = path
                    .strip_prefix(root)
                    .expect("walked entries remain beneath their root")
                    .to_str()
                    .ok_or_else(|| {
                        WombatError::configuration(format!(
                            "add directory entry `{}` is not valid UTF-8",
                            path.display()
                        ))
                    })?
                    .replace('\\', "/");
                let bytes = fs::read(&path).map_err(|error| WombatError::io(&path, error))?;
                let after =
                    fs::symlink_metadata(&path).map_err(|error| WombatError::io(&path, error))?;
                let fingerprint = crate::source::SourceFingerprint::from_metadata(&metadata);
                if crate::source::SourceFingerprint::from_metadata(&after) != fingerprint {
                    return Err(WombatError::configuration(format!(
                        "add directory entry `{}` changed while it was being read",
                        path.display()
                    )));
                }
                leaves.push(ImportedLeaf {
                    relative,
                    bytes,
                    fingerprint,
                    executable: is_executable(&after),
                });
            } else {
                return Err(WombatError::configuration(format!(
                    "add directory entry `{}` is not a regular file or directory",
                    path.display()
                )));
            }
        }
        Ok(())
    }

    let mut leaves = Vec::new();
    walk(root, root, &mut leaves)?;
    leaves.sort_by(|left, right| left.relative.cmp(&right.relative));
    Ok(leaves)
}

fn persist_directory_addition(
    root: &Path,
    source_root: &Path,
    target_root: &Path,
    desired: &[ImportedLeaf],
    existing: Option<&[ImportedLeaf]>,
    auto: Option<(&Path, &str, Option<&str>, &crate::source::SourceFingerprint)>,
) -> Result<()> {
    if snapshot_import_tree(target_root)? != desired {
        return Err(WombatError::configuration(format!(
            "add target directory `{}` changed after preflight",
            target_root.display()
        )));
    }
    if let Some(existing) = existing
        && snapshot_import_tree(source_root)? != existing
    {
        return Err(WombatError::configuration(format!(
            "source directory `{}` changed after preflight",
            source_root.display()
        )));
    }
    if let Some((path, old, _, fingerprint)) = auto {
        let metadata = fs::symlink_metadata(path).map_err(|error| WombatError::io(path, error))?;
        let current = fs::read_to_string(path).map_err(|error| WombatError::io(path, error))?;
        if &crate::source::SourceFingerprint::from_metadata(&metadata) != fingerprint
            || current != old
        {
            return Err(WombatError::configuration(format!(
                "`{AUTO_MODULE}` changed after add preflight"
            )));
        }
    }

    let existing_paths = existing
        .unwrap_or_default()
        .iter()
        .map(|leaf| leaf.relative.as_str())
        .collect::<BTreeSet<_>>();
    let mut created_directories = create_missing_parents(root, source_root)?;
    if !source_root.exists() {
        fs::create_dir(source_root).map_err(|error| WombatError::io(source_root, error))?;
        created_directories.push(source_root.to_path_buf());
    }
    let mut created_files = Vec::new();
    let result = (|| {
        for leaf in desired {
            if existing_paths.contains(leaf.relative.as_str()) {
                continue;
            }
            let destination = source_root.join(portable_path(&leaf.relative));
            let created =
                create_missing_parents(root, destination.parent().unwrap_or(source_root))?;
            created_directories.extend(created);
            let mut temporary =
                prepare_temp(destination.parent().unwrap_or(source_root), &leaf.bytes)?;
            set_import_permissions(temporary.as_file_mut(), leaf.executable)?;
            temporary
                .persist(&destination)
                .map_err(|error| WombatError::io(&destination, error.error))?;
            created_files.push(destination);
        }
        if let Some((path, _, Some(updated), _)) = auto {
            let metadata =
                fs::symlink_metadata(path).map_err(|error| WombatError::io(path, error))?;
            let temporary = prepare_temp(path.parent().unwrap_or(root), updated.as_bytes())?;
            temporary
                .as_file()
                .set_permissions(metadata.permissions())
                .map_err(|error| WombatError::io(temporary.path(), error))?;
            temporary
                .persist(path)
                .map_err(|error| WombatError::io(path, error.error))?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        for file in created_files.iter().rev() {
            let _ = fs::remove_file(file);
        }
        cleanup_directories(&created_directories);
        return Err(error);
    }
    Ok(())
}

fn source_path_for_home_directory(relative: &Path) -> Result<String> {
    let value = relative
        .to_str()
        .ok_or_else(|| WombatError::configuration("add target paths must be valid UTF-8"))?;
    let normalized = value.replace('\\', "/");
    if normalized.is_empty() {
        return Err(WombatError::configuration(
            "the target home itself cannot be added as a directory",
        ));
    }
    if normalized == ".config" {
        Ok("dot_config".to_string())
    } else if let Some(relative) = normalized.strip_prefix(".config/") {
        Ok(format!("dot_config/{relative}"))
    } else if normalized == ".local" {
        Ok("dot_local".to_string())
    } else if let Some(relative) = normalized.strip_prefix(".local/") {
        Ok(format!("dot_local/{relative}"))
    } else {
        Ok(format!("home/{normalized}"))
    }
}

fn portable_path(relative: &str) -> PathBuf {
    relative
        .split('/')
        .fold(PathBuf::new(), |path, component| path.join(component))
}

fn validate_target_components(home: &Path, target: &Path) -> Result<()> {
    let relative = target.strip_prefix(home).map_err(|_| {
        WombatError::configuration(format!(
            "add target `{}` must be beneath target home `{}`",
            target.display(),
            home.display()
        ))
    })?;
    let mut current = home.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        let metadata =
            fs::symlink_metadata(&current).map_err(|error| WombatError::io(&current, error))?;
        if metadata.file_type().is_symlink() {
            return Err(WombatError::configuration(format!(
                "add target path component `{}` must not be a symbolic link",
                current.display()
            )));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn set_import_permissions(file: &mut fs::File, executable: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = if executable { 0o755 } else { 0o644 };
    file.set_permissions(fs::Permissions::from_mode(mode))
        .map_err(|error| WombatError::io("<temporary add file>", error))
}

#[cfg(not(unix))]
fn set_import_permissions(_: &mut fs::File, _: bool) -> Result<()> {
    Ok(())
}

struct DirectoryCoverage<'a> {
    directory: &'a EvaluatedDirectory,
    target: TargetPath,
}

fn directory_coverages<'a>(
    directories: &'a [EvaluatedDirectory],
    source: &str,
) -> Result<Vec<DirectoryCoverage<'a>>> {
    let mut coverages = Vec::new();
    for directory in directories {
        let Some(relative) = source
            .strip_prefix(&directory.root)
            .and_then(|suffix| suffix.strip_prefix('/'))
        else {
            continue;
        };
        coverages.push(DirectoryCoverage {
            directory,
            target: expand_target_root(&directory.target_root, relative)?,
        });
    }
    Ok(coverages)
}

fn validate_prospective_outputs(
    requested_file: &Path,
    source: &str,
    artifacts: &[crate::manifest::EvaluatedArtifact],
    coverages: &[DirectoryCoverage<'_>],
    fallback: &TargetPath,
) -> Result<()> {
    let outputs = coverages
        .iter()
        .map(|coverage| &coverage.target)
        .collect::<Vec<_>>();
    for (index, left) in outputs.iter().enumerate() {
        if left.key() != fallback.key() && targets_overlap(left, fallback) {
            return Err(WombatError::configuration(format!(
                "cannot add `{}` because covering target `{}` overlaps requested target `{}`",
                requested_file.display(),
                left.display,
                fallback.display
            )));
        }
        if outputs[index + 1..]
            .iter()
            .any(|right| targets_overlap(left, right))
        {
            return Err(WombatError::configuration(format!(
                "cannot add `{}` because covering declarations produce overlapping prospective targets including `{}`",
                requested_file.display(),
                left.display
            )));
        }
        for artifact in artifacts {
            let is_existing_same_leaf = artifact.source == source
                && artifact.target.key() == left.key()
                && matches!(artifact.source_origin, SourceOrigin::Directory { .. });
            if targets_overlap(&artifact.target, left) && !is_existing_same_leaf {
                return Err(WombatError::configuration(format!(
                    "cannot add `{}` because target `{}` overlaps an artifact owned by `{}` from `{}` declared at {}",
                    requested_file.display(),
                    left.display,
                    artifact.owner,
                    artifact.source,
                    artifact.declared_at
                )));
            }
        }
    }
    Ok(())
}

fn targets_overlap(
    left: &crate::manifest::TargetPath,
    right: &crate::manifest::TargetPath,
) -> bool {
    if left.anchor != right.anchor {
        return false;
    }
    left.path == right.path
        || is_segment_ancestor(&left.path, &right.path)
        || is_segment_ancestor(&right.path, &left.path)
}

fn is_segment_ancestor(parent: &str, child: &str) -> bool {
    child
        .strip_prefix(parent)
        .is_some_and(|suffix| suffix.starts_with('/'))
}

fn source_path_for_home_file(relative: &Path) -> Result<String> {
    let value = relative
        .to_str()
        .ok_or_else(|| WombatError::configuration("add target paths must be valid UTF-8"))?;
    if value.is_empty() {
        return Err(WombatError::configuration(
            "the target home itself cannot be added as a file",
        ));
    }
    let normalized = value.replace('\\', "/");
    if normalized == ".config" {
        return Err(WombatError::configuration(
            "the target configuration anchor cannot be added as a file",
        ));
    }
    if let Some(config_relative) = normalized.strip_prefix(".config/") {
        Ok(format!("dot_config/{config_relative}"))
    } else if normalized == ".local" {
        Err(WombatError::configuration(
            "the target local-data anchor cannot be added as a file",
        ))
    } else if let Some(local_relative) = normalized.strip_prefix(".local/") {
        Ok(format!("dot_local/{local_relative}"))
    } else {
        Ok(format!("home/{normalized}"))
    }
}

fn validate_destination_path(root: &Path, destination: &Path) -> Result<()> {
    let relative = destination
        .strip_prefix(root)
        .expect("generated source destinations remain under the repository");
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(WombatError::configuration(
                "generated source destination contains invalid path components",
            ));
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(WombatError::configuration(format!(
                    "source destination `{}` must not contain symbolic links",
                    current.strip_prefix(root).unwrap_or(&current).display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(WombatError::io(&current, error)),
        }
    }
    Ok(())
}

fn parse_generated_region(source: &str) -> std::result::Result<BTreeSet<String>, String> {
    let (content_start, content_end) = generated_region_bounds(source)?;
    let body = &source[content_start..content_end];
    let mut entries = BTreeSet::new();
    let mut previous: Option<String> = None;
    for line in body.split_terminator('\n') {
        if line.is_empty() {
            return Err("the generated region contains a blank line".to_string());
        }
        let path = parse_generated_line(line)?;
        if generated_line(&path) != line {
            return Err(format!("non-canonical generated declaration `{line}`"));
        }
        if previous.as_ref().is_some_and(|prior| prior >= &path) {
            return Err("generated declarations must be unique and sorted".to_string());
        }
        previous = Some(path.clone());
        entries.insert(path);
    }
    if !body.is_empty() && !body.ends_with('\n') {
        return Err("the generated region must end each declaration with a newline".to_string());
    }
    Ok(entries)
}

fn generated_region_bounds(source: &str) -> std::result::Result<(usize, usize), String> {
    let begin_matches = source.match_indices(BEGIN_SENTINEL).collect::<Vec<_>>();
    let end_matches = source.match_indices(END_SENTINEL).collect::<Vec<_>>();
    if begin_matches.len() != 1 || end_matches.len() != 1 {
        return Err("expected exactly one intact `wombat:add` generated region".to_string());
    }
    let (begin, _) = begin_matches[0];
    let (end, _) = end_matches[0];
    let begin_line_start = source[..begin].rfind('\n').map_or(0, |index| index + 1);
    let begin_line_end = source[begin..]
        .find('\n')
        .map(|index| begin + index + 1)
        .ok_or_else(|| "the begin sentinel must end with a newline".to_string())?;
    let end_line_start = source[..end].rfind('\n').map_or(0, |index| index + 1);
    let end_line_end = source[end..]
        .find('\n')
        .map_or(source.len(), |index| end + index);
    if &source[begin_line_start..begin_line_end - 1] != BEGIN_SENTINEL
        || &source[end_line_start..end_line_end] != END_SENTINEL
        || end_line_start < begin_line_end
    {
        return Err("generated sentinels must occupy ordered lines by themselves".to_string());
    }
    Ok((begin_line_end, end_line_start))
}

fn render_generated_region(
    source: &str,
    entries: &BTreeSet<String>,
) -> std::result::Result<String, String> {
    let (start, end) = generated_region_bounds(source)?;
    let body = entries
        .iter()
        .map(|entry| generated_line(entry))
        .collect::<Vec<_>>()
        .join("\n");
    let body = if body.is_empty() {
        body
    } else {
        format!("{body}\n")
    };
    Ok(format!("{}{}{}", &source[..start], body, &source[end..]))
}

fn generated_line(path: &str) -> String {
    format!("w.install(\"{}\")", escape_lua_string(path))
}

fn escape_lua_string(value: &str) -> String {
    let mut escaped = String::new();
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character.is_control() && u32::from(character) <= 0xff => {
                escaped.push_str(&format!("\\x{:02x}", u32::from(character)));
            }
            character => escaped.push(character),
        }
    }
    escaped
}

fn parse_generated_line(line: &str) -> std::result::Result<String, String> {
    let inner = line
        .strip_prefix("w.install(\"")
        .and_then(|line| line.strip_suffix("\")"))
        .ok_or_else(|| format!("unsupported content `{line}` in generated region"))?;
    unescape_lua_string(inner)
}

fn unescape_lua_string(value: &str) -> std::result::Result<String, String> {
    let mut bytes = Vec::new();
    let mut chars = value.chars();
    while let Some(character) = chars.next() {
        if character != '\\' {
            let mut buffer = [0; 4];
            bytes.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
            continue;
        }
        match chars.next() {
            Some('\\') => bytes.push(b'\\'),
            Some('"') => bytes.push(b'"'),
            Some('n') => bytes.push(b'\n'),
            Some('r') => bytes.push(b'\r'),
            Some('t') => bytes.push(b'\t'),
            Some('x') => {
                let high = chars.next().and_then(|value| value.to_digit(16));
                let low = chars.next().and_then(|value| value.to_digit(16));
                let (Some(high), Some(low)) = (high, low) else {
                    return Err("invalid hexadecimal escape in generated declaration".to_string());
                };
                bytes.push(u8::try_from(high * 16 + low).expect("two hex digits fit in a byte"));
            }
            _ => return Err("invalid escape in generated declaration".to_string()),
        }
    }
    String::from_utf8(bytes)
        .map_err(|_| "generated declaration does not contain valid UTF-8".to_string())
}

fn persist_addition(
    root: &Path,
    source_path: &Path,
    source_bytes: Option<(&[u8], bool)>,
    auto_update: Option<(&Path, Option<&[u8]>, &fs::Metadata)>,
) -> Result<()> {
    let created_directories = create_missing_parents(root, source_path.parent().unwrap_or(root))?;
    let mut source_was_created = false;
    let result = (|| {
        let mut source_temp = source_bytes
            .map(|(bytes, _)| prepare_temp(source_path.parent().unwrap_or(root), bytes))
            .transpose()?;
        if let (Some(temporary), Some((_, executable))) = (&mut source_temp, source_bytes) {
            set_import_permissions(temporary.as_file_mut(), executable)?;
        }
        let auto_temp = auto_update
            .and_then(|(path, bytes, _)| bytes.map(|bytes| (path, bytes)))
            .map(|(path, bytes)| prepare_temp(path.parent().unwrap_or(root), bytes))
            .transpose()?;
        if let (Some(temp), Some((_, _, metadata))) = (&auto_temp, auto_update) {
            temp.as_file()
                .set_permissions(metadata.permissions())
                .map_err(|error| WombatError::io(temp.path(), error))?;
        }

        if let Some(temp) = source_temp {
            temp.persist(source_path)
                .map_err(|error| WombatError::io(source_path, error.error))?;
            source_was_created = true;
        }
        if let Some(temp) = auto_temp {
            let path = auto_update.expect("an auto temp requires an update").0;
            temp.persist(path)
                .map_err(|error| WombatError::io(path, error.error))?;
        }
        Ok(())
    })();

    if let Err(error) = result {
        if source_was_created {
            let _ = fs::remove_file(source_path);
        }
        cleanup_directories(&created_directories);
        return Err(error);
    }
    Ok(())
}

fn prepare_temp(directory: &Path, bytes: &[u8]) -> Result<NamedTempFile> {
    let mut temp =
        NamedTempFile::new_in(directory).map_err(|error| WombatError::io(directory, error))?;
    temp.write_all(bytes)
        .map_err(|error| WombatError::io(temp.path(), error))?;
    temp.as_file_mut()
        .sync_all()
        .map_err(|error| WombatError::io(temp.path(), error))?;
    Ok(temp)
}

fn create_missing_parents(root: &Path, parent: &Path) -> Result<Vec<PathBuf>> {
    let relative = parent
        .strip_prefix(root)
        .expect("source parents remain beneath the repository");
    let mut current = root.to_path_buf();
    let mut created = Vec::new();
    for component in relative.components() {
        current.push(component.as_os_str());
        let exists = match current.try_exists() {
            Ok(exists) => exists,
            Err(error) => {
                cleanup_directories(&created);
                return Err(WombatError::io(&current, error));
            }
        };
        if !exists {
            if let Err(error) = fs::create_dir(&current) {
                cleanup_directories(&created);
                return Err(WombatError::io(&current, error));
            }
            created.push(current.clone());
        }
    }
    Ok(created)
}

fn cleanup_directories(created: &[PathBuf]) {
    for directory in created.iter().rev() {
        let _ = fs::remove_dir(directory);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;

    use super::{
        escape_lua_string, generated_line, parse_generated_region, persist_directory_addition,
        render_generated_region, snapshot_import_tree, source_path_for_home_file,
        unescape_lua_string,
    };
    use std::path::Path;

    #[test]
    fn maps_home_paths_to_literal_source_anchors() {
        assert_eq!(
            source_path_for_home_file(Path::new(".config/starship.toml")).unwrap(),
            "dot_config/starship.toml"
        );
        assert_eq!(
            source_path_for_home_file(Path::new(".zshrc")).unwrap(),
            "home/.zshrc"
        );
    }

    #[test]
    fn lua_string_escaping_round_trips() {
        let value = "dot_config/a \\\"quote\\\"\n.toml";
        assert_eq!(
            unescape_lua_string(&escape_lua_string(value)).unwrap(),
            value
        );
        assert_eq!(generated_line("home/.zshrc"), "w.install(\"home/.zshrc\")");
    }

    #[test]
    fn generated_region_is_sorted_and_preserves_surrounding_lua() {
        let source = "local w = require(\"wombat\")\n\n-- wombat:add begin\nw.install(\"home/.zshrc\")\n-- wombat:add end\n\nreturn true\n";
        let parsed = parse_generated_region(source).unwrap();
        assert_eq!(parsed, BTreeSet::from(["home/.zshrc".to_string()]));
        let entries = BTreeSet::from([
            "home/.zshrc".to_string(),
            "dot_config/starship.toml".to_string(),
        ]);
        let rendered = render_generated_region(source, &entries).unwrap();
        assert!(
            rendered
                .contains("w.install(\"dot_config/starship.toml\")\nw.install(\"home/.zshrc\")")
        );
        assert!(rendered.ends_with("\nreturn true\n"));
    }

    #[cfg(unix)]
    #[test]
    fn directory_transaction_rolls_back_leaves_when_auto_publication_fails() {
        use std::os::unix::fs::PermissionsExt as _;

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("source");
        let target = temporary.path().join("target");
        let modules = root.join("modules");
        fs::create_dir_all(&modules).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(target.join("file"), "contents\n").unwrap();
        let auto = modules.join("auto.lua");
        let old = "-- wombat:add begin\n-- wombat:add end\n";
        fs::write(&auto, old).unwrap();
        let fingerprint =
            crate::source::SourceFingerprint::from_metadata(&fs::symlink_metadata(&auto).unwrap());
        let leaves = snapshot_import_tree(&target).unwrap();
        fs::set_permissions(&modules, fs::Permissions::from_mode(0o555)).unwrap();

        let destination = root.join("dot_config/tree");
        let result = persist_directory_addition(
            &root,
            &destination,
            &target,
            &leaves,
            None,
            Some((&auto, old, Some("updated\n"), &fingerprint)),
        );
        fs::set_permissions(&modules, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(result.is_err());
        assert!(!destination.exists());
        assert_eq!(fs::read_to_string(auto).unwrap(), old);
    }
}
