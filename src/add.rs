use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use tempfile::NamedTempFile;

use crate::manifest::InferenceBasis;
use crate::path::{
    infer_target, prefixed_source, reject_legacy_config_tree, validate_relative_path,
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
}

impl AddOutcome {
    pub fn display(&self) -> String {
        match self.status {
            AddStatus::Added => format!("added `{}` through module `auto`", self.source),
            AddStatus::DeclarationAdded => {
                format!(
                    "declared existing source `{}` through module `auto`",
                    self.source
                )
            }
            AddStatus::AlreadyPresent => {
                format!("already added `{}` through module `auto`", self.source)
            }
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
    reject_legacy_config_tree(&root)?;
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
    if !target_metadata.file_type().is_file() {
        return Err(WombatError::configuration(format!(
            "add target `{}` must be a regular file",
            target.display()
        )));
    }
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

    let manifest = evaluate(&root)?;
    if !manifest.modules.iter().any(|module| module.name == "auto") {
        return Err(WombatError::configuration(
            "module `auto` is not selected; add `w.use(\"auto\")` to root policy before using `wombat add`",
        ));
    }
    let (source_anchor, target_relative) = prefixed_source(&source)?
        .expect("generated add sources always contain a recognized anchor prefix");
    let prospective_target =
        infer_target(source_anchor, target_relative, InferenceBasis::SourcePrefix)?;
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
                artifact.declared_from
            )));
        }
    }

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
        if existing != target_bytes {
            return Err(WombatError::configuration(format!(
                "source state `{source}` already exists with different contents; overwrite and re-add are not supported in this slice"
            )));
        }
    }

    let declaration_added = generated.insert(source.clone());
    if source_exists && !declaration_added {
        return Ok(AddOutcome {
            status: AddStatus::AlreadyPresent,
            source,
        });
    }

    let updated_auto = render_generated_region(&auto_source, &generated)
        .expect("a parsed generated region can always be rendered");
    persist_addition(
        &root,
        &source_path,
        (!source_exists).then_some(target_bytes.as_slice()),
        &auto_path,
        declaration_added.then_some(updated_auto.as_bytes()),
        &auto_metadata,
    )?;

    Ok(AddOutcome {
        status: if source_exists {
            AddStatus::DeclarationAdded
        } else {
            AddStatus::Added
        },
        source,
    })
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
    source_bytes: Option<&[u8]>,
    auto_path: &Path,
    auto_bytes: Option<&[u8]>,
    auto_metadata: &fs::Metadata,
) -> Result<()> {
    let created_directories = create_missing_parents(root, source_path.parent().unwrap_or(root))?;
    let mut source_was_created = false;
    let result = (|| {
        let source_temp = source_bytes
            .map(|bytes| prepare_temp(source_path.parent().unwrap_or(root), bytes))
            .transpose()?;
        let auto_temp = auto_bytes
            .map(|bytes| prepare_temp(auto_path.parent().unwrap_or(root), bytes))
            .transpose()?;
        if let Some(temp) = &auto_temp {
            temp.as_file()
                .set_permissions(auto_metadata.permissions())
                .map_err(|error| WombatError::io(temp.path(), error))?;
        }

        if let Some(temp) = source_temp {
            temp.persist(source_path)
                .map_err(|error| WombatError::io(source_path, error.error))?;
            source_was_created = true;
        }
        if let Some(temp) = auto_temp {
            temp.persist(auto_path)
                .map_err(|error| WombatError::io(auto_path, error.error))?;
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

    use super::{
        escape_lua_string, generated_line, parse_generated_region, render_generated_region,
        source_path_for_home_file, unescape_lua_string,
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
}
