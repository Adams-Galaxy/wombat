use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

use mlua::{Lua, Table, Value};

use crate::frozen::FrozenValue;
use crate::manifest::{
    Artifact, ArtifactKind, Dependency, DependencyKind, Manifest, ManifestModule,
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
}

impl ModuleRecord {
    fn selected() -> Self {
        Self {
            explicit_config: None,
            state: EvaluationState::Selected,
            export: None,
        }
    }

    fn config(&self) -> FrozenValue {
        self.explicit_config
            .as_ref()
            .map_or_else(FrozenValue::empty_map, |config| config.value.clone())
    }
}

#[derive(Debug)]
struct RuntimeState {
    root: PathBuf,
    modules: BTreeMap<String, ModuleRecord>,
    dependencies: BTreeSet<Dependency>,
    artifacts: Vec<Artifact>,
    stack: Vec<String>,
}

impl RuntimeState {
    fn active_module(&self) -> Option<&str> {
        self.stack.last().map(String::as_str)
    }

    fn active_source_base(&self) -> PathBuf {
        self.active_module().map_or_else(
            || self.root.clone(),
            |module| self.root.join("modules").join(module),
        )
    }
}

pub fn build(root: &Path) -> Result<Manifest> {
    let root = fs::canonicalize(root).map_err(|source| WombatError::io(root, source))?;
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

    Ok(build_manifest(&state.borrow()))
}

fn configure_package_path(lua: &Lua, root: &Path) -> Result<()> {
    let package: Table = lua.globals().get("package")?;
    let current: String = package.get("path")?;
    let root = root.to_string_lossy();
    package.set("path", format!("{root}/?.lua;{root}/?/init.lua;{current}"))?;
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
        lua.create_function(move |lua, (source_path, target): (String, String)| {
            let location = caller_location(lua, &state);
            register_artifact(&state, &source_path, &target, location)
                .map_err(mlua::Error::external)
        })?,
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
    target: &str,
    location: Location,
) -> Result<()> {
    validate_relative_source(source_path)?;
    let target = normalize_target(target)?;

    let mut state = state.borrow_mut();
    let absolute_source = state.active_source_base().join(source_path);
    if !absolute_source.is_file() {
        return Err(WombatError::configuration(format!(
            "static artifact source `{}` does not exist or is not a file",
            display_path(&state.root, &absolute_source)
        )));
    }

    let owner = state.active_module().unwrap_or(ROOT_MODULE).to_string();
    let source = display_path(&state.root, &absolute_source);
    state.artifacts.push(Artifact {
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
            .state = EvaluationState::Evaluating;
        state.stack.push(name.to_string());
    }

    let path = state
        .borrow()
        .root
        .join("modules")
        .join(name)
        .join("init.lua");
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

fn build_manifest(state: &RuntimeState) -> Manifest {
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
    artifacts.sort();

    Manifest {
        format_version: 1,
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

fn validate_relative_source(source: &str) -> Result<()> {
    let path = Path::new(source);
    if source.is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            !matches!(component, Component::Normal(_)) || matches!(component, Component::ParentDir)
        })
    {
        return Err(WombatError::configuration(format!(
            "invalid static artifact source `{source}`; expected a relative path without traversal"
        )));
    }
    Ok(())
}

fn normalize_target(target: &str) -> Result<String> {
    let Some(relative) = target.strip_prefix("~/") else {
        return Err(WombatError::configuration(format!(
            "invalid target `{target}`; explicit targets must begin with `~/`"
        )));
    };
    let segments = relative.split('/').collect::<Vec<_>>();
    if segments.is_empty()
        || segments.iter().any(|segment| {
            segment.is_empty() || *segment == "." || *segment == ".." || segment.contains('\\')
        })
    {
        return Err(WombatError::configuration(format!(
            "invalid target `{target}`; target paths must not be empty, traverse, or contain empty components"
        )));
    }
    Ok(format!("~/{}", segments.join("/")))
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
    use super::{normalize_target, validate_module_name, validate_relative_source};

    #[test]
    fn validates_initial_module_names() {
        assert!(validate_module_name("theme-2").is_ok());
        assert!(validate_module_name("themes.kanagawa").is_err());
        assert!(validate_module_name("../theme").is_err());
    }

    #[test]
    fn validates_relative_sources() {
        assert!(validate_relative_source("starship.toml").is_ok());
        assert!(validate_relative_source("config/starship.toml").is_ok());
        assert!(validate_relative_source("../starship.toml").is_err());
        assert!(validate_relative_source("/tmp/starship.toml").is_err());
    }

    #[test]
    fn normalizes_explicit_home_targets() {
        assert_eq!(
            normalize_target("~/.config/starship.toml").unwrap(),
            "~/.config/starship.toml"
        );
        assert!(normalize_target(".config/starship.toml").is_err());
        assert!(normalize_target("~/.config/../secret").is_err());
    }
}
