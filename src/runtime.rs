use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use mlua::{Lua, Table, Value};

use crate::frozen::FrozenValue;
use crate::manifest::{
    ArtifactKind, Dependency, DependencyKind, EvaluatedArtifact, EvaluatedManifest, InferenceBasis,
    ManifestModule, SourceAnchor,
};
use crate::path::{
    infer_target, parse_explicit_target, prefixed_source, reject_legacy_config_tree,
    validate_relative_path,
};
use crate::{Result, WombatError};

const WOMBAT_LUA: &str = include_str!("../lua/wombat/init.lua");
const ROOT_MODULE: &str = "<root>";

#[derive(Clone, Debug, Eq, PartialEq)]
struct Location {
    file: String,
    line: i64,
}

impl Location {
    fn display(&self) -> String {
        if self.line > 0 {
            format!("{}:{}", self.file, self.line)
        } else {
            self.file.clone()
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EvaluationState {
    Selected,
    Evaluating,
    Evaluated,
    Failed,
}

#[derive(Clone, Debug)]
struct ExplicitConfig {
    value: FrozenValue,
    locations: Vec<Location>,
}

#[derive(Clone, Debug)]
struct ModuleRecord {
    explicit_config: Option<ExplicitConfig>,
    state: EvaluationState,
    export: Option<FrozenValue>,
    location: Option<ModuleLocation>,
}

impl ModuleRecord {
    fn selected() -> Self {
        Self {
            explicit_config: None,
            state: EvaluationState::Selected,
            export: None,
            location: None,
        }
    }

    fn config(&self) -> FrozenValue {
        self.explicit_config
            .as_ref()
            .map_or_else(FrozenValue::empty_map, |config| config.value.clone())
    }
}

#[derive(Clone, Debug)]
struct ModuleLocation {
    file: PathBuf,
    source_base: PathBuf,
    source_anchor: Option<SourceAnchor>,
}

#[derive(Debug)]
struct RuntimeState {
    root: PathBuf,
    modules: BTreeMap<String, ModuleRecord>,
    dependencies: BTreeSet<Dependency>,
    artifacts: Vec<EvaluatedArtifact>,
    stack: Vec<String>,
}

impl RuntimeState {
    fn active_module(&self) -> Option<&str> {
        self.stack.last().map(String::as_str)
    }

    fn active_location(&self) -> (PathBuf, Option<SourceAnchor>) {
        self.active_module().map_or_else(
            || (self.root.clone(), None),
            |module| {
                let location = self
                    .modules
                    .get(module)
                    .and_then(|record| record.location.as_ref())
                    .expect("an active module must have a resolved location");
                (location.source_base.clone(), location.source_anchor)
            },
        )
    }
}

pub(crate) fn evaluate(root: &Path) -> Result<EvaluatedManifest> {
    let root = fs::canonicalize(root).map_err(|source| WombatError::io(root, source))?;
    reject_legacy_config_tree(&root)?;
    let entrypoint = root.join("wombat.lua");
    let source = read_utf8(&entrypoint)?;

    let lua = Lua::new();
    let state = Rc::new(RefCell::new(RuntimeState {
        root: root.clone(),
        modules: BTreeMap::new(),
        dependencies: BTreeSet::new(),
        artifacts: Vec::new(),
        stack: Vec::new(),
    }));

    configure_package_path(&lua, &root)?;
    register_preloaded_modules(&lua, Rc::clone(&state))?;

    lua.load(&source)
        .set_name(entrypoint.to_string_lossy())
        .exec()?;

    evaluate_selected_modules(&lua, &state)?;
    validate_dependency_cycles(&state.borrow())?;
    validate_artifact_conflicts(&state.borrow().artifacts)?;

    Ok(build_manifest(&state.borrow()))
}

fn configure_package_path(lua: &Lua, root: &Path) -> Result<()> {
    let package: Table = lua.globals().get("package")?;
    let library = root.join("lua").to_string_lossy().replace('\\', "/");
    package.set("path", format!("{library}/?.lua;{library}/?/init.lua"))?;
    Ok(())
}

fn register_preloaded_modules(lua: &Lua, state: Rc<RefCell<RuntimeState>>) -> Result<()> {
    let package: Table = lua.globals().get("package")?;
    let preload: Table = package.get("preload")?;
    let native = create_native_module(lua, state)?;

    preload.set(
        "_wombat",
        lua.create_function(move |_, ()| Ok(native.clone()))?,
    )?;
    preload.set(
        "wombat",
        lua.create_function(|lua, ()| {
            lua.load(WOMBAT_LUA)
                .set_name("<wombat>/init.lua")
                .eval::<Table>()
        })?,
    )?;
    Ok(())
}

fn create_native_module(lua: &Lua, state: Rc<RefCell<RuntimeState>>) -> Result<Table> {
    let native = lua.create_table()?;

    let use_state = Rc::clone(&state);
    native.set(
        "use_module",
        lua.create_function(move |lua, (name, config): (String, Value)| {
            let location = caller_location(lua, &use_state);
            register_selection(&use_state, &name, config, location).map_err(mlua::Error::external)
        })?,
    )?;

    let using_state = Rc::clone(&state);
    native.set(
        "using_module",
        lua.create_function(move |lua, name: String| {
            let location = caller_location(lua, &using_state);
            consume_module(lua, &using_state, &name, location).map_err(mlua::Error::external)
        })?,
    )?;

    let config_state = Rc::clone(&state);
    native.set(
        "module_config",
        lua.create_function(move |lua, ()| {
            current_module_config(lua, &config_state).map_err(mlua::Error::external)
        })?,
    )?;

    native.set(
        "install_file",
        lua.create_function(
            move |lua, (source_path, target): (String, Option<String>)| {
                let location = caller_location(lua, &state);
                register_artifact(&state, &source_path, target.as_deref(), location)
                    .map_err(mlua::Error::external)
            },
        )?,
    )?;

    Ok(native)
}

fn register_selection(
    state: &Rc<RefCell<RuntimeState>>,
    name: &str,
    config: Value,
    location: Location,
) -> Result<()> {
    validate_module_name(name)?;

    let mut state = state.borrow_mut();
    let from = state.active_module().unwrap_or(ROOT_MODULE).to_string();
    let is_module_selection = state.active_module().is_some();
    let explicit_config = if config.is_nil() {
        None
    } else {
        if is_module_selection {
            return Err(WombatError::configuration(format!(
                "module `{from}` cannot configure module `{name}` at {}; configuration-bearing use() belongs to root policy",
                location.display()
            )));
        }
        Some(FrozenValue::from_lua(config)?)
    };

    state.dependencies.insert(Dependency {
        kind: DependencyKind::Use,
        from,
        to: name.to_string(),
        declared_from: location.file.clone(),
    });

    let record = state
        .modules
        .entry(name.to_string())
        .or_insert_with(ModuleRecord::selected);

    if let Some(value) = explicit_config {
        match &mut record.explicit_config {
            Some(existing) if existing.value == value => existing.locations.push(location),
            Some(existing) => {
                let prior = existing
                    .locations
                    .iter()
                    .map(Location::display)
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(WombatError::configuration(format!(
                    "conflicting configuration for module `{name}`: first selected at {prior}; conflicting selection at {}",
                    location.display()
                )));
            }
            None => {
                record.explicit_config = Some(ExplicitConfig {
                    value,
                    locations: vec![location],
                });
            }
        }
    }

    Ok(())
}

fn consume_module(
    lua: &Lua,
    state: &Rc<RefCell<RuntimeState>>,
    name: &str,
    location: Location,
) -> Result<Value> {
    validate_module_name(name)?;

    {
        let mut state = state.borrow_mut();
        let Some(from) = state.active_module().map(str::to_owned) else {
            return Err(WombatError::configuration(
                "using() may only be called while evaluating a Wombat module",
            ));
        };
        if !state.modules.contains_key(name) {
            return Err(WombatError::configuration(format!(
                "module `{from}` uses module `{name}`, but `{name}` was not selected with use()"
            )));
        }
        state.dependencies.insert(Dependency {
            kind: DependencyKind::Using,
            from,
            to: name.to_string(),
            declared_from: location.file,
        });
    }

    evaluate_module(lua, state, name)?;

    let export = state
        .borrow()
        .modules
        .get(name)
        .and_then(|module| module.export.clone())
        .ok_or_else(|| {
            WombatError::configuration(format!(
                "module `{name}` finished without a resolved public export"
            ))
        })?;
    export.to_lua(lua).map_err(WombatError::from)
}

fn current_module_config(lua: &Lua, state: &Rc<RefCell<RuntimeState>>) -> Result<Value> {
    let state = state.borrow();
    let name = state.active_module().ok_or_else(|| {
        WombatError::configuration(
            "module.config() may only be called while evaluating a Wombat module",
        )
    })?;
    let config = state
        .modules
        .get(name)
        .expect("the active module must exist in the registry")
        .config();
    config.to_lua(lua).map_err(WombatError::from)
}

fn register_artifact(
    state: &Rc<RefCell<RuntimeState>>,
    source_path: &str,
    explicit_target: Option<&str>,
    location: Location,
) -> Result<()> {
    validate_relative_path(source_path, "static artifact source")?;

    let mut state = state.borrow_mut();
    let (source_base, module_anchor) = state.active_location();
    let prefixed = if module_anchor.is_none() {
        prefixed_source(source_path)?
    } else {
        None
    };
    let (inferred_anchor, inferred_path, inference_basis) = match (module_anchor, prefixed) {
        (Some(anchor), _) => (
            Some(anchor),
            source_path,
            Some(InferenceBasis::ModuleAnchor),
        ),
        (None, Some((anchor, relative))) => {
            (Some(anchor), relative, Some(InferenceBasis::SourcePrefix))
        }
        (None, None) => (None, source_path, None),
    };
    let target = match explicit_target {
        Some(target) => parse_explicit_target(target)?,
        None => {
            let anchor = inferred_anchor.ok_or_else(|| {
                WombatError::configuration(format!(
                    "cannot infer a target for source `{source_path}` from an anchorless module; use a `dot_config/` or `home/` source prefix, or provide `to`"
                ))
            })?;
            infer_target(
                anchor,
                inferred_path,
                inference_basis.expect("an inferred anchor has an inference basis"),
            )?
        }
    };

    let absolute_source = source_base.join(source_path);
    validate_regular_source(&state.root, &source_base, &absolute_source)?;

    let owner = state.active_module().unwrap_or(ROOT_MODULE).to_string();
    let source = display_path(&state.root, &absolute_source);
    state.artifacts.push(EvaluatedArtifact {
        kind: ArtifactKind::File,
        source,
        target,
        owner,
        declared_from: location.file,
    });
    Ok(())
}

fn evaluate_selected_modules(lua: &Lua, state: &Rc<RefCell<RuntimeState>>) -> Result<()> {
    loop {
        let next = state.borrow().modules.iter().find_map(|(name, module)| {
            (module.state == EvaluationState::Selected).then(|| name.clone())
        });
        let Some(name) = next else {
            break;
        };
        evaluate_module(lua, state, &name)?;
    }
    Ok(())
}

fn evaluate_module(lua: &Lua, state: &Rc<RefCell<RuntimeState>>, name: &str) -> Result<()> {
    let resolved_location = {
        let state = state.borrow();
        state
            .modules
            .get(name)
            .and_then(|module| module.location.clone())
            .map_or_else(|| resolve_module(&state.root, name), Ok)?
    };

    {
        let mut state = state.borrow_mut();
        let module = state.modules.get(name).ok_or_else(|| {
            WombatError::configuration(format!("module `{name}` was not selected"))
        })?;
        match module.state {
            EvaluationState::Evaluated => return Ok(()),
            EvaluationState::Evaluating => {
                let start = state
                    .stack
                    .iter()
                    .position(|active| active == name)
                    .unwrap_or(0);
                let mut cycle = state.stack[start..].to_vec();
                cycle.push(name.to_string());
                return Err(WombatError::configuration(format!(
                    "module cycle: {}",
                    cycle.join(" -> ")
                )));
            }
            EvaluationState::Failed => {
                return Err(WombatError::configuration(format!(
                    "module `{name}` previously failed to evaluate"
                )));
            }
            EvaluationState::Selected => {}
        }

        state
            .modules
            .get_mut(name)
            .expect("module was checked above")
            .location = Some(resolved_location.clone());
        state
            .modules
            .get_mut(name)
            .expect("module was checked above")
            .state = EvaluationState::Evaluating;
        state.stack.push(name.to_string());
    }

    let path = resolved_location.file;
    let result = read_utf8(&path).and_then(|source| {
        let value = lua
            .load(&source)
            .set_name(path.to_string_lossy())
            .eval::<Value>()?;
        FrozenValue::from_lua(value)
    });

    let mut state = state.borrow_mut();
    let popped = state.stack.pop();
    debug_assert_eq!(popped.as_deref(), Some(name));
    let module = state
        .modules
        .get_mut(name)
        .expect("an evaluating module must remain registered");
    match result {
        Ok(export) => {
            module.export = Some(export);
            module.state = EvaluationState::Evaluated;
            Ok(())
        }
        Err(error) => {
            module.state = EvaluationState::Failed;
            Err(error)
        }
    }
}

fn resolve_module(root: &Path, name: &str) -> Result<ModuleLocation> {
    let candidates = [
        (
            root.join("modules").join(format!("{name}.lua")),
            root.to_path_buf(),
            None,
        ),
        (
            root.join("modules")
                .join("dot_config")
                .join(format!("{name}.lua")),
            root.join("dot_config"),
            Some(SourceAnchor::DotConfig),
        ),
        (
            root.join("modules")
                .join("home")
                .join(format!("{name}.lua")),
            root.join("home"),
            Some(SourceAnchor::Home),
        ),
    ];
    let matches = candidates
        .iter()
        .filter(|(file, _, _)| file.is_file())
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [(file, source_base, source_anchor)] => Ok(ModuleLocation {
            file: file.clone(),
            source_base: source_base.clone(),
            source_anchor: *source_anchor,
        }),
        [] => {
            let searched = candidates
                .iter()
                .map(|(file, _, _)| display_path(root, file))
                .collect::<Vec<_>>()
                .join(", ");
            Err(WombatError::configuration(format!(
                "module `{name}` was not found; searched {searched}"
            )))
        }
        _ => {
            let found = matches
                .iter()
                .map(|(file, _, _)| display_path(root, file))
                .collect::<Vec<_>>()
                .join(", ");
            Err(WombatError::configuration(format!(
                "module `{name}` is ambiguous across module anchors: {found}"
            )))
        }
    }
}

fn validate_dependency_cycles(state: &RuntimeState) -> Result<()> {
    let mut graph: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for dependency in &state.dependencies {
        if dependency.from != ROOT_MODULE {
            graph
                .entry(&dependency.from)
                .or_default()
                .insert(&dependency.to);
        }
    }

    let mut complete = BTreeSet::new();
    let mut stack = Vec::new();
    for module in state.modules.keys() {
        visit_dependency(module, &graph, &mut complete, &mut stack)?;
    }
    Ok(())
}

fn visit_dependency<'a>(
    module: &'a str,
    graph: &BTreeMap<&'a str, BTreeSet<&'a str>>,
    complete: &mut BTreeSet<&'a str>,
    stack: &mut Vec<&'a str>,
) -> Result<()> {
    if complete.contains(module) {
        return Ok(());
    }
    if let Some(start) = stack.iter().position(|active| *active == module) {
        let mut cycle = stack[start..].to_vec();
        cycle.push(module);
        return Err(WombatError::configuration(format!(
            "module cycle: {}",
            cycle.join(" -> ")
        )));
    }

    stack.push(module);
    if let Some(dependencies) = graph.get(module) {
        for dependency in dependencies {
            visit_dependency(dependency, graph, complete, stack)?;
        }
    }
    stack.pop();
    complete.insert(module);
    Ok(())
}

fn build_manifest(state: &RuntimeState) -> EvaluatedManifest {
    let modules = state
        .modules
        .iter()
        .map(|(name, module)| ManifestModule {
            name: name.clone(),
            config: module.config(),
        })
        .collect();
    let dependencies = state.dependencies.iter().cloned().collect();
    let mut artifacts = state.artifacts.clone();
    artifacts.sort_by(|left, right| {
        left.target
            .key()
            .cmp(&right.target.key())
            .then_with(|| left.owner.cmp(&right.owner))
            .then_with(|| left.source.cmp(&right.source))
            .then_with(|| left.declared_from.cmp(&right.declared_from))
    });

    EvaluatedManifest {
        modules,
        dependencies,
        artifacts,
    }
}

fn validate_module_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(WombatError::configuration(format!(
            "invalid module name `{name}`; expected ASCII letters, numbers, `_`, or `-`"
        )));
    }
    Ok(())
}

fn validate_regular_source(root: &Path, base: &Path, source: &Path) -> Result<()> {
    source
        .strip_prefix(base)
        .expect("validated relative sources remain under their base");
    let relative = source
        .strip_prefix(root)
        .expect("artifact source bases remain under the repository");
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(WombatError::configuration(format!(
                    "static artifact source `{}` does not exist or is not a regular file",
                    display_path(root, source)
                )));
            }
            Err(error) => return Err(WombatError::io(&current, error)),
        };
        if metadata.file_type().is_symlink() {
            return Err(WombatError::configuration(format!(
                "static artifact source `{}` must not contain symbolic links",
                display_path(root, source)
            )));
        }
    }
    let metadata = fs::symlink_metadata(source).map_err(|error| WombatError::io(source, error))?;
    if !metadata.file_type().is_file() {
        return Err(WombatError::configuration(format!(
            "static artifact source `{}` does not exist or is not a regular file",
            display_path(root, source)
        )));
    }
    Ok(())
}

fn validate_artifact_conflicts(artifacts: &[EvaluatedArtifact]) -> Result<()> {
    let mut ordered = artifacts.iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| {
        left.target
            .key()
            .cmp(&right.target.key())
            .then_with(|| left.owner.cmp(&right.owner))
            .then_with(|| left.source.cmp(&right.source))
    });

    for (index, artifact) in ordered.iter().enumerate() {
        let duplicates = ordered
            .iter()
            .filter(|candidate| candidate.target.key() == artifact.target.key())
            .copied()
            .collect::<Vec<_>>();
        if duplicates.len() > 1
            && ordered[..index]
                .iter()
                .all(|prior| prior.target.key() != artifact.target.key())
        {
            return Err(artifact_conflict(
                &artifact.target.display,
                "multiple artifacts resolve to the same target",
                &duplicates,
            ));
        }

        let descendants = ordered
            .iter()
            .skip(index + 1)
            .filter(|descendant| {
                artifact.target.anchor == descendant.target.anchor
                    && is_path_ancestor(&artifact.target.path, &descendant.target.path)
            })
            .copied()
            .collect::<Vec<_>>();
        if !descendants.is_empty() {
            let displays = descendants
                .iter()
                .map(|descendant| format!("`{}`", descendant.target.display))
                .collect::<Vec<_>>()
                .join(", ");
            let mut conflicts = Vec::with_capacity(descendants.len() + 1);
            conflicts.push(*artifact);
            conflicts.extend(descendants);
            return Err(artifact_conflict(
                &artifact.target.display,
                &format!("file target is an ancestor of {displays}"),
                &conflicts,
            ));
        }
    }
    Ok(())
}

fn is_path_ancestor(parent: &str, child: &str) -> bool {
    child
        .strip_prefix(parent)
        .is_some_and(|suffix| suffix.starts_with('/'))
}

fn artifact_conflict(target: &str, reason: &str, artifacts: &[&EvaluatedArtifact]) -> WombatError {
    let declarations = artifacts
        .iter()
        .map(|artifact| {
            format!(
                "{} from `{}` declared at {}",
                artifact.owner, artifact.source, artifact.declared_from
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    WombatError::configuration(format!(
        "artifact conflict at `{target}`: {reason}; declarations: {declarations}"
    ))
}

fn caller_location(lua: &Lua, state: &Rc<RefCell<RuntimeState>>) -> Location {
    let raw = lua.inspect_stack(2, |debug| {
        let source = debug
            .source()
            .source
            .map_or_else(|| "<unknown>".to_string(), |source| source.into_owned());
        let line = debug
            .current_line()
            .and_then(|line| i64::try_from(line).ok())
            .unwrap_or(0);
        (source, line)
    });
    let (source, line) = raw.unwrap_or_else(|| ("<unknown>".to_string(), 0));
    let source = source.strip_prefix('@').unwrap_or(&source);
    Location {
        file: display_path(&state.borrow().root, Path::new(source)),
        line,
    }
}

fn display_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn read_utf8(path: &Path) -> Result<String> {
    fs::read_to_string(path).map_err(|source| WombatError::io(path, source))
}

#[cfg(test)]
mod tests {
    use super::{is_path_ancestor, validate_module_name};

    #[test]
    fn validates_initial_module_names() {
        assert!(validate_module_name("theme-2").is_ok());
        assert!(validate_module_name("themes.kanagawa").is_err());
        assert!(validate_module_name("../theme").is_err());
    }

    #[test]
    fn detects_only_segment_ancestor_paths() {
        assert!(is_path_ancestor("nvim", "nvim/init.lua"));
        assert!(!is_path_ancestor("nvim", "nvim-old/init.lua"));
        assert!(!is_path_ancestor("nvim", "nvim"));
    }
}
