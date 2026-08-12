use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use tempfile::{Builder, NamedTempFile};

use crate::model::manifest::{EvaluatedDirectory, EvaluatedTargetOrigin};
use crate::model::path::{join_relative, reject_legacy_artifact_trees, validate_relative_path};
use crate::model::selection::{
    compile_selector, encode_target_path, hidden_components_authorized, is_excluded, matcher,
    project_physical,
};
use crate::model::source::SourceFingerprint;
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
                "{action} `{}` through selection `{declared_source}` owned by module `{owner}`",
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct ImportedLeaf {
    relative: String,
    bytes: Vec<u8>,
    executable: bool,
}

#[derive(Clone, Debug)]
struct AutoUpdate {
    path: PathBuf,
    original: String,
    updated: Option<String>,
    fingerprint: SourceFingerprint,
    permissions: fs::Permissions,
}

pub fn add(root: &Path, target_root: &Path, requested: &Path) -> Result<AddOutcome> {
    if !requested.is_absolute() {
        return Err(WombatError::configuration(format!(
            "add target `{}` must be absolute",
            requested.display()
        )));
    }
    let requested_metadata =
        fs::symlink_metadata(requested).map_err(|error| WombatError::io(requested, error))?;
    if requested_metadata.file_type().is_symlink() {
        return Err(WombatError::configuration(format!(
            "add target `{}` must not be a symbolic link",
            requested.display()
        )));
    }
    if !requested_metadata.file_type().is_file() && !requested_metadata.file_type().is_dir() {
        return Err(WombatError::configuration(
            "wombat add accepts only regular files and directories",
        ));
    }

    let root = fs::canonicalize(root).map_err(|error| WombatError::io(root, error))?;
    reject_legacy_artifact_trees(&root)?;
    validate_target_components(target_root, requested)?;
    let target_root =
        fs::canonicalize(target_root).map_err(|error| WombatError::io(target_root, error))?;
    let target = fs::canonicalize(requested).map_err(|error| WombatError::io(requested, error))?;
    let relative = target.strip_prefix(&target_root).map_err(|_| {
        WombatError::configuration(format!(
            "add target `{}` must be a strict descendant of target root `{}`",
            target.display(),
            target_root.display()
        ))
    })?;
    if relative.as_os_str().is_empty() {
        return Err(WombatError::configuration(
            "add target must be a strict descendant of target root",
        ));
    }
    let target_relative = portable(relative, "add target path")?;
    validate_relative_path(&target_relative, "add target path")?;

    let leaves = snapshot_import(&target)?;
    if requested_metadata.file_type().is_dir() && leaves.is_empty() {
        return Err(WombatError::configuration(format!(
            "add target directory `{}` contains no regular files",
            target.display()
        )));
    }
    let encoded = encode_target_path(&target_relative)?;
    let source = format!("src/{encoded}");
    let destination = root.join(&source);
    validate_destination(&root, &destination)?;
    let source_exists = destination
        .try_exists()
        .map_err(|error| WombatError::io(&destination, error))?;
    if source_exists {
        compare_snapshot(
            &destination,
            requested_metadata.file_type().is_dir(),
            &leaves,
        )?;
    }

    let evaluated = crate::lua::evaluate(&root)?;
    let coverage = reconcile_coverage(
        &evaluated.directories,
        &evaluated.artifacts,
        &source,
        &target_relative,
        requested_metadata.file_type().is_dir(),
        &leaves,
    )?;

    let (method, auto_update) = if let Some((owner, declared_source)) = coverage {
        (
            AddMethod::Directory {
                owner,
                declared_source,
            },
            None,
        )
    } else {
        let update = prepare_auto_update(&root, &evaluated, &target_relative)?;
        (AddMethod::GeneratedAuto, Some(update))
    };

    let declaration_added = auto_update
        .as_ref()
        .is_some_and(|update| update.updated.is_some());
    if source_exists && !declaration_added {
        return Ok(AddOutcome {
            status: AddStatus::AlreadyPresent,
            source,
            method,
        });
    }

    persist(
        &root,
        &destination,
        requested_metadata.file_type().is_dir(),
        (!source_exists).then_some(leaves.as_slice()),
        auto_update.as_ref(),
    )?;

    Ok(AddOutcome {
        status: if source_exists {
            AddStatus::DeclarationAdded
        } else {
            AddStatus::Added
        },
        source,
        method,
    })
}

fn reconcile_coverage(
    directories: &[EvaluatedDirectory],
    artifacts: &[crate::model::manifest::EvaluatedArtifact],
    source_root: &str,
    target_root: &str,
    directory: bool,
    leaves: &[ImportedLeaf],
) -> Result<Option<(String, String)>> {
    let mut identity: Option<(String, String)> = None;
    let mut uncovered = false;
    let leaves = leaves.iter().map(|leaf| {
        let source = if directory {
            format!("{source_root}/{}", leaf.relative)
        } else {
            source_root.to_string()
        };
        let target = if directory {
            format!("{target_root}/{}", leaf.relative)
        } else {
            target_root.to_string()
        };
        (source, target)
    });

    for (source, target) in leaves {
        let mut captured = Vec::new();
        for selection in directories {
            if let Some(mapped) = prospective_target(selection, &source)? {
                captured.push((selection, mapped));
            }
        }
        if captured.len() > 1 {
            let owners = captured
                .iter()
                .map(|(selection, _)| {
                    format!(
                        "`{}` owned by `{}` at {}",
                        selection.declared_source, selection.owner, selection.declared_at
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            return Err(WombatError::configuration(format!(
                "cannot add target `{target}` because prospective source `{source}` has ambiguous selection coverage: {owners}"
            )));
        }
        let leaf_identity = if let Some((selection, mapped)) = captured.first() {
            if mapped != &target {
                return Err(WombatError::configuration(format!(
                    "cannot add target `{target}` because selection `{}` owned by `{}` would map prospective source `{source}` to `{mapped}`",
                    selection.declared_source, selection.owner
                )));
            }
            Some((selection.owner.clone(), selection.declared_source.clone()))
        } else {
            None
        };
        match (&identity, leaf_identity) {
            (None, Some(_)) if uncovered => {
                return Err(WombatError::configuration(format!(
                    "cannot add `{target_root}` because its prospective leaves have partial selection coverage"
                )));
            }
            (None, Some(current)) => identity = Some(current),
            (Some(expected), Some(current)) if expected == &current => {}
            (Some(_), Some(_)) | (Some(_), None) => {
                return Err(WombatError::configuration(format!(
                    "cannot add `{target_root}` because its prospective leaves have partial or different selection coverage"
                )));
            }
            (None, None) => uncovered = true,
        }

        for artifact in artifacts {
            let same_existing = artifact.source == source && artifact.target.path == target;
            if !same_existing && paths_overlap(&artifact.target.path, &target) {
                return Err(WombatError::configuration(format!(
                    "cannot add target `{target}` because it overlaps artifact `{}` owned by `{}` from `{}` declared at {}",
                    artifact.target.path, artifact.owner, artifact.source, artifact.declared_at
                )));
            }
        }
    }
    Ok(identity)
}

fn prospective_target(selection: &EvaluatedDirectory, source: &str) -> Result<Option<String>> {
    let Some(relative) = source
        .strip_prefix(&selection.root)
        .and_then(|suffix| suffix.strip_prefix('/'))
    else {
        return Ok(None);
    };
    let exclusions = selection
        .exclusions
        .iter()
        .map(|value| {
            compile_selector(value, selection.hidden)
                .and_then(|selector| matcher(&selector.physical))
        })
        .collect::<Result<Vec<_>>>()?;
    if is_excluded(&exclusions, relative, false) {
        return Ok(None);
    }
    if selection.glob {
        if !hidden_components_authorized(relative, &selection.physical_selector)
            || !matcher(&selection.physical_selector)?.is_match(relative)
        {
            return Ok(None);
        }
    } else if relative
        .split('/')
        .any(crate::model::selection::is_hidden_component)
    {
        return Ok(None);
    }

    let Some(target_root) = &selection.target_root else {
        return Err(WombatError::configuration(format!(
            "selection `{}` would capture prospective source `{source}` without an allocated target",
            selection.declared_source
        )));
    };
    let physical = match target_root.origin {
        EvaluatedTargetOrigin::Explicit { .. } if selection.glob => relative
            .strip_prefix(selection.static_root.trim_end_matches('/'))
            .unwrap_or(relative)
            .trim_start_matches('/')
            .to_string(),
        EvaluatedTargetOrigin::Explicit { .. } => relative.to_string(),
        EvaluatedTargetOrigin::Inferred { .. } if selection.glob => relative.to_string(),
        EvaluatedTargetOrigin::Inferred { .. } => {
            join_relative(&selection.physical_selector, relative)
        }
    };
    let projection = project_physical(
        &physical,
        hidden_components_authorized(&physical, &selection.physical_selector),
    )?;
    if !projection.allocated {
        return Err(WombatError::configuration(format!(
            "selection `{}` would capture prospective source `{source}` as unallocated",
            selection.declared_source
        )));
    }
    let mut target = join_relative(&target_root.path, &projection.logical);
    if physical.ends_with(".tmpl") {
        target = target.strip_suffix(".tmpl").unwrap_or(&target).to_string();
    }
    Ok(Some(target))
}

fn prepare_auto_update(
    root: &Path,
    evaluated: &crate::model::manifest::EvaluatedManifest,
    target: &str,
) -> Result<AutoUpdate> {
    let path = root.join(AUTO_MODULE);
    let metadata = fs::symlink_metadata(&path).map_err(|_| {
        WombatError::configuration(format!(
            "`{AUTO_MODULE}` is required before `wombat add`; run `wombat init` or create and select the standard generated module"
        ))
    })?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(WombatError::configuration(format!(
            "`{AUTO_MODULE}` must be a regular non-symlink file"
        )));
    }
    if !evaluated.modules.iter().any(|module| module.name == "auto") {
        return Err(WombatError::configuration(
            "module `auto` is not selected; add `w.use(\"auto\")` before using `wombat add`",
        ));
    }
    let original = fs::read_to_string(&path).map_err(|error| WombatError::io(&path, error))?;
    let mut entries = parse_generated_region(&original).map_err(|message| {
        WombatError::configuration(format!("cannot update `{AUTO_MODULE}`: {message}"))
    })?;
    let added = entries.insert(target.to_string());
    let updated = added.then(|| {
        render_generated_region(&original, &entries)
            .expect("a parsed generated region can always be rendered")
    });
    Ok(AutoUpdate {
        path,
        original,
        updated,
        fingerprint: SourceFingerprint::from_metadata(&metadata),
        permissions: metadata.permissions(),
    })
}

fn snapshot_import(path: &Path) -> Result<Vec<ImportedLeaf>> {
    let metadata = fs::symlink_metadata(path).map_err(|error| WombatError::io(path, error))?;
    if metadata.file_type().is_file() {
        return Ok(vec![ImportedLeaf {
            relative: String::new(),
            bytes: fs::read(path).map_err(|error| WombatError::io(path, error))?,
            executable: executable(&metadata),
        }]);
    }
    if !metadata.file_type().is_dir() {
        return Err(WombatError::configuration(
            "wombat add accepts only regular files and directories",
        ));
    }
    let mut leaves = Vec::new();
    walk_import(path, path, &mut leaves)?;
    Ok(leaves)
}

fn walk_import(root: &Path, directory: &Path, leaves: &mut Vec<ImportedLeaf>) -> Result<()> {
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
            walk_import(root, &path, leaves)?;
        } else if metadata.file_type().is_file() {
            leaves.push(ImportedLeaf {
                relative: portable(
                    path.strip_prefix(root)
                        .expect("walked entry remains beneath root"),
                    "add directory entry",
                )?,
                bytes: fs::read(&path).map_err(|error| WombatError::io(&path, error))?,
                executable: executable(&metadata),
            });
        } else {
            return Err(WombatError::configuration(format!(
                "add directory entry `{}` must be a regular file or directory",
                path.display()
            )));
        }
    }
    leaves.sort_by(|left, right| left.relative.cmp(&right.relative));
    Ok(())
}

fn compare_snapshot(destination: &Path, directory: bool, expected: &[ImportedLeaf]) -> Result<()> {
    let metadata =
        fs::symlink_metadata(destination).map_err(|error| WombatError::io(destination, error))?;
    if metadata.file_type().is_symlink()
        || metadata.file_type().is_dir() != directory
        || metadata.file_type().is_file() == directory
    {
        return Err(WombatError::configuration(format!(
            "source `{}` already exists with a different type",
            destination.display()
        )));
    }
    let actual = snapshot_import(destination)?;
    if actual != expected {
        return Err(WombatError::configuration(format!(
            "source `{}` already exists with different contents, executable intent, or directory membership",
            destination.display()
        )));
    }
    Ok(())
}

fn persist(
    root: &Path,
    destination: &Path,
    directory: bool,
    source: Option<&[ImportedLeaf]>,
    auto: Option<&AutoUpdate>,
) -> Result<()> {
    let created_parents = create_missing_parents(root, destination.parent().unwrap_or(root))?;
    let mut published_source = false;
    let result = (|| {
        let prepared_source = source
            .map(|leaves| prepare_source(destination, directory, leaves))
            .transpose()?;
        let mut prepared_auto = auto
            .and_then(|update| update.updated.as_ref().map(|contents| (update, contents)))
            .map(|(update, contents)| prepare_file(&update.path, contents.as_bytes(), false))
            .transpose()?;
        if let (Some(temporary), Some(update)) = (&mut prepared_auto, auto) {
            temporary
                .as_file()
                .set_permissions(update.permissions.clone())
                .map_err(|error| WombatError::io(temporary.path(), error))?;
        }

        if let Some(prepared) = prepared_source {
            publish_source(prepared, destination)?;
            published_source = true;
        }
        if let (Some(mut temporary), Some(update)) = (prepared_auto, auto) {
            let current_metadata = fs::symlink_metadata(&update.path)
                .map_err(|error| WombatError::io(&update.path, error))?;
            let current = fs::read_to_string(&update.path)
                .map_err(|error| WombatError::io(&update.path, error))?;
            if SourceFingerprint::from_metadata(&current_metadata) != update.fingerprint
                || current != update.original
            {
                return Err(WombatError::configuration(format!(
                    "`{AUTO_MODULE}` changed during add; source publication was rolled back"
                )));
            }
            temporary
                .as_file_mut()
                .sync_all()
                .map_err(|error| WombatError::io(temporary.path(), error))?;
            temporary
                .persist(&update.path)
                .map_err(|error| WombatError::io(&update.path, error.error))?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        if published_source {
            if directory {
                let _ = fs::remove_dir_all(destination);
            } else {
                let _ = fs::remove_file(destination);
            }
        }
        cleanup_parents(&created_parents);
        return Err(error);
    }
    Ok(())
}

enum PreparedSource {
    File(NamedTempFile),
    Directory(PathBuf),
}

fn prepare_source(
    destination: &Path,
    directory: bool,
    leaves: &[ImportedLeaf],
) -> Result<PreparedSource> {
    if !directory {
        let leaf = leaves.first().expect("a regular-file import has one leaf");
        return prepare_file(destination, &leaf.bytes, leaf.executable).map(PreparedSource::File);
    }
    let parent = destination
        .parent()
        .expect("source destinations have parents");
    let temporary = Builder::new()
        .prefix(".wombat-add-")
        .tempdir_in(parent)
        .map_err(|error| WombatError::io(parent, error))?;
    for leaf in leaves {
        let path = portable_join(temporary.path(), &leaf.relative);
        let parent = path.parent().expect("import leaves have a parent");
        fs::create_dir_all(parent).map_err(|error| WombatError::io(parent, error))?;
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .map_err(|error| WombatError::io(&path, error))?;
        file.write_all(&leaf.bytes)
            .map_err(|error| WombatError::io(&path, error))?;
        set_permissions(&file, leaf.executable, &path)?;
        file.sync_all()
            .map_err(|error| WombatError::io(&path, error))?;
    }
    Ok(PreparedSource::Directory(temporary.keep()))
}

fn prepare_file(destination: &Path, bytes: &[u8], executable: bool) -> Result<NamedTempFile> {
    let parent = destination.parent().expect("source files have parents");
    let mut temporary =
        NamedTempFile::new_in(parent).map_err(|error| WombatError::io(parent, error))?;
    temporary
        .write_all(bytes)
        .map_err(|error| WombatError::io(temporary.path(), error))?;
    set_permissions(temporary.as_file(), executable, temporary.path())?;
    temporary
        .as_file_mut()
        .sync_all()
        .map_err(|error| WombatError::io(temporary.path(), error))?;
    Ok(temporary)
}

fn publish_source(prepared: PreparedSource, destination: &Path) -> Result<()> {
    match prepared {
        PreparedSource::File(temporary) => temporary
            .persist_noclobber(destination)
            .map(|_| ())
            .map_err(|error| WombatError::io(destination, error.error)),
        PreparedSource::Directory(temporary) => {
            fs::rename(&temporary, destination).map_err(|error| {
                let _ = fs::remove_dir_all(&temporary);
                WombatError::io(destination, error)
            })
        }
    }
}

fn create_missing_parents(root: &Path, parent: &Path) -> Result<Vec<PathBuf>> {
    let relative = parent
        .strip_prefix(root)
        .expect("source destinations remain beneath the repository");
    let mut current = root.to_path_buf();
    let mut created = Vec::new();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(WombatError::configuration(
                "generated source destination contains invalid path components",
            ));
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() => {
                cleanup_parents(&created);
                return Err(WombatError::configuration(format!(
                    "source destination component `{}` must be a directory, not a symlink or file",
                    current.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Err(error) = fs::create_dir(&current) {
                    cleanup_parents(&created);
                    return Err(WombatError::io(&current, error));
                }
                created.push(current.clone());
            }
            Err(error) => {
                cleanup_parents(&created);
                return Err(WombatError::io(&current, error));
            }
        }
    }
    Ok(created)
}

fn cleanup_parents(paths: &[PathBuf]) {
    for path in paths.iter().rev() {
        let _ = fs::remove_dir(path);
    }
}

fn validate_target_components(root: &Path, requested: &Path) -> Result<()> {
    let relative = requested.strip_prefix(root).map_err(|_| {
        WombatError::configuration(format!(
            "add target `{}` must be a strict descendant of target root `{}`",
            requested.display(),
            root.display()
        ))
    })?;
    if relative.as_os_str().is_empty() {
        return Err(WombatError::configuration(
            "add target must be a strict descendant of target root",
        ));
    }
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(WombatError::configuration(
                "add target contains invalid path components",
            ));
        };
        current.push(component);
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

fn validate_destination(root: &Path, destination: &Path) -> Result<()> {
    let relative = destination
        .strip_prefix(root)
        .expect("encoded source remains beneath repository");
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(WombatError::configuration(format!(
                    "source destination `{}` must not contain symbolic links",
                    current.display()
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
    let (start, end) = generated_bounds(source)?;
    let body = &source[start..end];
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

fn generated_bounds(source: &str) -> std::result::Result<(usize, usize), String> {
    let begins = source.match_indices(BEGIN_SENTINEL).collect::<Vec<_>>();
    let ends = source.match_indices(END_SENTINEL).collect::<Vec<_>>();
    if begins.len() != 1 || ends.len() != 1 {
        return Err("expected exactly one intact `wombat:add` generated region".to_string());
    }
    let begin = begins[0].0;
    let end = ends[0].0;
    let begin_line = source[..begin].rfind('\n').map_or(0, |index| index + 1);
    let start = source[begin..]
        .find('\n')
        .map(|index| begin + index + 1)
        .ok_or_else(|| "the begin sentinel must end with a newline".to_string())?;
    let end_line = source[..end].rfind('\n').map_or(0, |index| index + 1);
    let end_finish = source[end..]
        .find('\n')
        .map_or(source.len(), |index| end + index);
    if &source[begin_line..start - 1] != BEGIN_SENTINEL
        || &source[end_line..end_finish] != END_SENTINEL
        || end_line < start
    {
        return Err("generated sentinels must occupy ordered lines by themselves".to_string());
    }
    Ok((start, end_line))
}

fn render_generated_region(
    source: &str,
    entries: &BTreeSet<String>,
) -> std::result::Result<String, String> {
    let (start, end) = generated_bounds(source)?;
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
    format!("w.install(\"{}\")", escape_lua(path))
}

fn parse_generated_line(line: &str) -> std::result::Result<String, String> {
    let value = line
        .strip_prefix("w.install(\"")
        .and_then(|line| line.strip_suffix("\")"))
        .ok_or_else(|| format!("unsupported content `{line}` in generated region"))?;
    unescape_lua(value)
}

fn escape_lua(value: &str) -> String {
    value
        .chars()
        .flat_map(|character| match character {
            '\\' => "\\\\".chars().collect::<Vec<_>>(),
            '"' => "\\\"".chars().collect(),
            '\n' => "\\n".chars().collect(),
            '\r' => "\\r".chars().collect(),
            '\t' => "\\t".chars().collect(),
            value => vec![value],
        })
        .collect()
}

fn unescape_lua(value: &str) -> std::result::Result<String, String> {
    let mut output = String::new();
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            output.push(character);
            continue;
        }
        output.push(match characters.next() {
            Some('\\') => '\\',
            Some('"') => '"',
            Some('n') => '\n',
            Some('r') => '\r',
            Some('t') => '\t',
            _ => return Err("invalid escape in generated declaration".to_string()),
        });
    }
    Ok(output)
}

fn paths_overlap(left: &str, right: &str) -> bool {
    left == right
        || left
            .strip_prefix(right)
            .is_some_and(|suffix| suffix.starts_with('/'))
        || right
            .strip_prefix(left)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn portable(path: &Path, subject: &str) -> Result<String> {
    path.to_str()
        .map(|value| value.replace('\\', "/"))
        .ok_or_else(|| WombatError::configuration(format!("{subject} must be valid UTF-8")))
}

fn portable_join(root: &Path, relative: &str) -> PathBuf {
    relative
        .split('/')
        .fold(root.to_path_buf(), |path, component| path.join(component))
}

#[cfg(unix)]
fn executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn set_permissions(file: &fs::File, executable: bool, path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    file.set_permissions(fs::Permissions::from_mode(if executable {
        0o755
    } else {
        0o644
    }))
    .map_err(|error| WombatError::io(path, error))
}

#[cfg(not(unix))]
fn set_permissions(_: &fs::File, _: bool, _: &Path) -> Result<()> {
    Ok(())
}
