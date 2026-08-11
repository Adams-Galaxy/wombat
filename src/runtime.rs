use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use mlua::{Function, Lua, MultiValue, Table, Value};
use sha2::{Digest, Sha256};

use crate::context::{HostContext, ResolvedTarget, TargetOrigin, TargetPlatform};
use crate::frozen::FrozenValue;
use crate::inputs::{self, InputSpec};
use crate::manifest::{
    ArtifactKind, BuildInput, Dependency, DependencyKind, EvaluatedArtifact, EvaluatedDirectory,
    EvaluatedManifest, EvaluatedProduction, InferenceBasis, MAX_SOURCE_TRACE_FRAMES,
    ManifestModule, Observation, ObservationSubject, SourceAnchor, SourceFile, SourceLocation,
    SourceOrigin, SourceTrace,
};
use crate::path::{
    expand_target_root, infer_target, infer_target_root, parse_explicit_target,
    parse_explicit_target_root, prefixed_source, reject_noncanonical_artifact_trees,
    validate_declared_source,
};
use crate::source::{
    SourceFingerprint, join_portable, snapshot_directory, validate_source_components,
};
use crate::{Diagnostic, Result, WombatError};

const WOMBAT_LUA: &str = include_str!("../lua/wombat/init.lua");
const ROOT_MODULE: &str = "<root>";

#[derive(Clone, Debug, Eq, PartialEq)]
struct Location {
    trace: SourceTrace,
}

impl Location {
    fn display(&self) -> String {
        self.trace.to_string()
    }
}

#[derive(Clone, Debug)]
struct TrackedSource {
    manifest: SourceFile,
    fingerprint: SourceFingerprint,
    snapshot: String,
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
    sources: BTreeMap<String, TrackedSource>,
    modules: BTreeMap<String, ModuleRecord>,
    dependencies: BTreeSet<Dependency>,
    artifacts: Vec<EvaluatedArtifact>,
    directories: Vec<EvaluatedDirectory>,
    stack: Vec<String>,
    host: HostContext,
    target: ResolvedTarget,
    target_override: Option<Location>,
    target_first_read: Option<Location>,
    root_policy_started: bool,
    project_arguments: Vec<OsString>,
    input_specs: BTreeMap<u64, InputSpec>,
    next_input_spec: u64,
    inputs_declared: bool,
    inputs: Vec<BuildInput>,
    observations: BTreeMap<(ObservationSubject, String), Observation>,
    project_help: Option<String>,
    failure_frames: Vec<SourceLocation>,
    failure_tail_call: bool,
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

#[derive(Clone, Debug)]
pub(crate) struct EvaluationOptions {
    pub project_arguments: Vec<OsString>,
    pub host: HostContext,
}

#[derive(Clone, Debug)]
pub(crate) enum EvaluationOutcome {
    Manifest(Box<EvaluatedManifest>),
    ProjectHelp(String),
}

pub(crate) fn evaluate(root: &Path) -> Result<EvaluatedManifest> {
    let outcome = evaluate_with(
        root,
        EvaluationOptions {
            project_arguments: Vec::new(),
            host: HostContext::observe()?,
        },
    )?;
    match outcome {
        EvaluationOutcome::Manifest(manifest) => Ok(*manifest),
        EvaluationOutcome::ProjectHelp(_) => Err(WombatError::configuration(
            "project help was requested during build evaluation",
        )),
    }
}

pub(crate) fn evaluate_with(root: &Path, options: EvaluationOptions) -> Result<EvaluationOutcome> {
    let root = fs::canonicalize(root).map_err(|source| WombatError::io(root, source))?;
    reject_noncanonical_artifact_trees(&root)?;
    let entrypoint = root.join("wombat.lua");

    let target = options.host.resolved_target();
    let lua = Lua::new();
    let state = Rc::new(RefCell::new(RuntimeState {
        root: root.clone(),
        sources: BTreeMap::new(),
        modules: BTreeMap::new(),
        dependencies: BTreeSet::new(),
        artifacts: Vec::new(),
        directories: Vec::new(),
        stack: Vec::new(),
        host: options.host,
        target,
        target_override: None,
        target_first_read: None,
        root_policy_started: false,
        project_arguments: options.project_arguments,
        input_specs: BTreeMap::new(),
        next_input_spec: 1,
        inputs_declared: false,
        inputs: Vec::new(),
        observations: BTreeMap::new(),
        project_help: None,
        failure_frames: Vec::new(),
        failure_tail_call: false,
    }));

    let source = load_tracked_source(&state, &entrypoint)?;

    configure_package_path(&lua, &root, Rc::clone(&state))?;
    register_preloaded_modules(&lua, Rc::clone(&state))?;

    let execution = execute_tracked_chunk(&lua, &state, &source, &entrypoint);

    if let Err(error) = execution {
        let state = state.borrow();
        if let Some(help) = &state.project_help {
            return Ok(EvaluationOutcome::ProjectHelp(help.clone()));
        }
        return Err(error);
    }

    {
        let state = state.borrow();
        if !state.inputs_declared && !state.project_arguments.is_empty() {
            return Err(WombatError::configuration(
                "project build arguments were provided, but this repository does not declare w.inputs()",
            ));
        }
        if let Some(help) = &state.project_help {
            return Ok(EvaluationOutcome::ProjectHelp(help.clone()));
        }
    }

    evaluate_selected_modules(&lua, &state)?;
    validate_dependency_cycles(&state.borrow())?;
    validate_artifact_conflicts(&state.borrow().artifacts)?;

    Ok(EvaluationOutcome::Manifest(Box::new(build_manifest(
        &state.borrow(),
    ))))
}

fn configure_package_path(lua: &Lua, root: &Path, state: Rc<RefCell<RuntimeState>>) -> Result<()> {
    let package: Table = lua.globals().get("package")?;
    let library = root.join("lua").to_string_lossy().replace('\\', "/");
    package.set("path", format!("{library}/?.lua;{library}/?/init.lua"))?;
    let existing: Table = package.get("searchers")?;
    let searchers = lua.create_table()?;
    searchers.set(1, existing.get::<Value>(1)?)?;
    let helper_root = root.join("lua");
    searchers.set(
        2,
        lua.create_function(move |lua, name: String| {
            let relative = helper_module_path(&name).map_err(mlua::Error::external)?;
            let candidates = [
                helper_root.join(format!("{relative}.lua")),
                helper_root.join(&relative).join("init.lua"),
            ];
            let Some(path) = candidates.iter().find(|path| path.is_file()) else {
                return Ok(MultiValue::from_vec(vec![Value::String(
                    lua.create_string(format!("\n\tno repository Lua module '{}'", name))?,
                )]));
            };
            let source = load_tracked_source(&state, path).map_err(mlua::Error::external)?;
            let loader = lua
                .load(&source)
                .set_name(format!("@{}", path.to_string_lossy()))
                .into_function()?;
            Ok(MultiValue::from_vec(vec![
                Value::Function(loader),
                Value::String(lua.create_string(display_path(&state.borrow().root, path))?),
            ]))
        })?,
    )?;
    package.set("searchers", searchers)?;
    Ok(())
}

fn helper_module_path(name: &str) -> Result<String> {
    if name.is_empty()
        || name.split('.').any(|segment| {
            segment.is_empty()
                || !segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        })
    {
        return Err(WombatError::configuration(format!(
            "invalid repository Lua module name `{name}`"
        )));
    }
    Ok(name.replace('.', "/"))
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

    let spec_state = Rc::clone(&state);
    native.set(
        "input_spec",
        lua.create_function(move |lua, (kind, options): (String, Value)| {
            let location = caller_location(lua, &spec_state);
            register_input_spec(&spec_state, &kind, options, location)
                .map_err(mlua::Error::external)
        })?,
    )?;

    let inputs_state = Rc::clone(&state);
    native.set(
        "resolve_inputs",
        lua.create_function(move |lua, schema: Table| {
            let location = caller_location(lua, &inputs_state);
            resolve_inputs(lua, &inputs_state, schema, location).map_err(mlua::Error::external)
        })?,
    )?;

    let host_state = Rc::clone(&state);
    native.set(
        "host_context",
        lua.create_function(move |lua, ()| {
            create_context_proxy(
                lua,
                Rc::clone(&host_state),
                ObservationSubject::Host,
                String::new(),
                false,
            )
        })?,
    )?;

    let target_state = Rc::clone(&state);
    native.set(
        "target_context",
        lua.create_function(move |lua, ()| {
            create_context_proxy(
                lua,
                Rc::clone(&target_state),
                ObservationSubject::Target,
                String::new(),
                true,
            )
        })?,
    )?;

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
        "install_path",
        lua.create_function(
            move |lua, (source_path, target, kind, context): (String, Option<String>, String, Value)| {
                let location = caller_location(lua, &state);
                register_artifact(&state, &source_path, target.as_deref(), &kind, context, location)
                    .map_err(mlua::Error::external)
            },
        )?,
    )?;

    Ok(native)
}

fn register_input_spec(
    state: &Rc<RefCell<RuntimeState>>,
    kind: &str,
    options: Value,
    location: Location,
) -> Result<u64> {
    let mut state = state.borrow_mut();
    if state.active_module().is_some() {
        return Err(WombatError::configuration(format!(
            "Wombat modules cannot declare project build inputs at {}",
            location.display()
        )));
    }
    if state.inputs_declared || state.root_policy_started {
        return Err(WombatError::configuration(format!(
            "input constructors must run before w.inputs(), use(), using(), or install() at {}",
            location.display()
        )));
    }
    let order = state.next_input_spec;
    state.next_input_spec = state
        .next_input_spec
        .checked_add(1)
        .ok_or_else(|| WombatError::configuration("too many project input declarations"))?;
    let spec = InputSpec::parse(order, kind, FrozenValue::from_lua(options)?, location.trace)?;
    state.input_specs.insert(order, spec);
    Ok(order)
}

fn resolve_inputs(
    lua: &Lua,
    state: &Rc<RefCell<RuntimeState>>,
    schema: Table,
    location: Location,
) -> Result<Table> {
    let pairs = schema
        .pairs::<Value, Value>()
        .collect::<mlua::Result<Vec<_>>>()?;
    let mut entries = Vec::with_capacity(pairs.len());
    for (name, descriptor) in pairs {
        let Value::String(name) = name else {
            return Err(WombatError::configuration(
                "w.inputs() schema keys must be strings",
            ));
        };
        let Value::Integer(descriptor) = descriptor else {
            return Err(WombatError::configuration(
                "w.inputs() values must be created by w.input constructors",
            ));
        };
        entries.push((
            name.to_str()?.to_string(),
            u64::try_from(descriptor)
                .map_err(|_| WombatError::configuration("invalid Wombat input descriptor"))?,
        ));
    }

    let resolved = {
        let mut state = state.borrow_mut();
        if state.active_module().is_some() {
            return Err(WombatError::configuration(format!(
                "w.inputs() belongs to root policy and cannot run in a module at {}",
                location.display()
            )));
        }
        if state.inputs_declared {
            return Err(WombatError::configuration(format!(
                "w.inputs() may be declared only once; repeated at {}",
                location.display()
            )));
        }
        if state.root_policy_started {
            return Err(WombatError::configuration(format!(
                "w.inputs() must run before use(), using(), install(), or target-dependent policy at {}",
                location.display()
            )));
        }
        let resolved = inputs::resolve(
            entries,
            &state.input_specs,
            &state.project_arguments,
            &state.host,
        )?;
        state.inputs_declared = true;
        state.inputs = resolved.manifest.clone();
        state.project_help = resolved.help.clone();
        resolved
    };
    if resolved.help.is_some() {
        return Err(WombatError::ProjectHelpRequested);
    }
    create_values_proxy(lua, resolved.values)
}

fn create_values_proxy(lua: &Lua, values: BTreeMap<String, FrozenValue>) -> Result<Table> {
    let proxy = lua.create_table()?;
    let metatable = lua.create_table()?;
    metatable.set(
        "__index",
        lua.create_function(move |lua, (_table, key): (Table, String)| {
            values
                .get(&key)
                .map_or(Ok(Value::Nil), |value| value.to_lua(lua))
        })?,
    )?;
    metatable.set(
        "__newindex",
        lua.create_function(|_, (_table, key, _value): (Table, Value, Value)| {
            Err::<(), _>(mlua::Error::external(WombatError::configuration(format!(
                "resolved build inputs are immutable; cannot assign `{key:?}`"
            ))))
        })?,
    )?;
    metatable.set("__metatable", false)?;
    proxy.set_metatable(Some(metatable))?;
    Ok(proxy)
}

fn create_context_proxy(
    lua: &Lua,
    state: Rc<RefCell<RuntimeState>>,
    subject: ObservationSubject,
    path: String,
    callable_target: bool,
) -> mlua::Result<Table> {
    let proxy = lua.create_table()?;
    let metatable = lua.create_table()?;
    let index_state = Rc::clone(&state);
    metatable.set(
        "__index",
        lua.create_function(move |lua, (_table, key): (Table, String)| {
            let location = caller_location(lua, &index_state);
            let child_path = if path.is_empty() {
                key
            } else {
                format!("{path}.{key}")
            };
            context_access(lua, &index_state, subject, &child_path, location)
                .map_err(mlua::Error::external)
        })?,
    )?;
    metatable.set(
        "__newindex",
        lua.create_function(|_, (_table, key, _value): (Table, Value, Value)| {
            Err::<(), _>(mlua::Error::external(WombatError::configuration(format!(
                "Wombat context is immutable; cannot assign `{key:?}`"
            ))))
        })?,
    )?;
    if callable_target {
        let call_state = Rc::clone(&state);
        metatable.set(
            "__call",
            lua.create_function(move |lua, (_table, value): (Table, Value)| {
                let location = caller_location(lua, &call_state);
                set_target(&call_state, value, location)?;
                Ok(_table)
            })?,
        )?;
    }
    metatable.set("__metatable", false)?;
    proxy.set_metatable(Some(metatable))?;
    Ok(proxy)
}

fn context_access(
    lua: &Lua,
    state: &Rc<RefCell<RuntimeState>>,
    subject: ObservationSubject,
    path: &str,
    location: Location,
) -> Result<Value> {
    let (value, missing) = {
        let mut state = state.borrow_mut();
        if subject == ObservationSubject::Target && state.target_first_read.is_none() {
            state.target_first_read = Some(location);
        }
        let root = match subject {
            ObservationSubject::Host => state.host.to_frozen(),
            ObservationSubject::Target => effective_target(&state).to_frozen(),
        };
        let found = frozen_at_path(&root, path).cloned();
        let missing = found.is_none();
        let value = found.unwrap_or(FrozenValue::Null);
        if !matches!(value, FrozenValue::Map(_)) && !is_foundational_target(subject, path) {
            state
                .observations
                .entry((subject, path.to_string()))
                .or_insert_with(|| Observation {
                    subject,
                    path: path.to_string(),
                    value: value.clone(),
                });
        }
        (value, missing)
    };
    if missing {
        return Ok(Value::Nil);
    }
    if matches!(value, FrozenValue::Map(_)) {
        create_context_proxy(lua, Rc::clone(state), subject, path.to_string(), false)
            .map(Value::Table)
            .map_err(WombatError::from)
    } else {
        readonly_frozen(lua, value).map_err(WombatError::from)
    }
}

fn readonly_frozen(lua: &Lua, value: FrozenValue) -> mlua::Result<Value> {
    match value {
        FrozenValue::Array(values) => {
            let proxy = lua.create_table()?;
            let metatable = lua.create_table()?;
            let length = values.len();
            metatable.set(
                "__index",
                lua.create_function(move |lua, (_table, index): (Table, i64)| {
                    usize::try_from(index)
                        .ok()
                        .and_then(|index| index.checked_sub(1))
                        .and_then(|index| values.get(index).cloned())
                        .map_or(Ok(Value::Nil), |value| readonly_frozen(lua, value))
                })?,
            )?;
            metatable.set("__len", lua.create_function(move |_, _: Table| Ok(length))?)?;
            metatable.set(
                "__newindex",
                lua.create_function(|_, (_table, _key, _value): (Table, Value, Value)| {
                    Err::<(), _>(mlua::Error::external(WombatError::configuration(
                        "Wombat context arrays are immutable",
                    )))
                })?,
            )?;
            metatable.set("__metatable", false)?;
            proxy.set_metatable(Some(metatable))?;
            Ok(Value::Table(proxy))
        }
        other => other.to_lua(lua),
    }
}

fn frozen_at_path<'a>(root: &'a FrozenValue, path: &str) -> Option<&'a FrozenValue> {
    path.split('.')
        .try_fold(root, |value, component| match value {
            FrozenValue::Map(map) => map.get(component),
            _ => None,
        })
}

fn is_foundational_target(subject: ObservationSubject, path: &str) -> bool {
    subject == ObservationSubject::Target && matches!(path, "os.name" | "arch")
}

fn effective_target(state: &RuntimeState) -> TargetPlatform {
    let target = &state.target.platform;
    if target.os.name == state.host.platform.os.name
        && target.arch == state.host.platform.arch
        && target.os.version.is_none()
        && target.os.kernel.is_none()
        && target.os.distribution.is_none()
    {
        state.host.platform.clone()
    } else {
        target.clone()
    }
}

fn set_target(
    state: &Rc<RefCell<RuntimeState>>,
    value: Value,
    location: Location,
) -> mlua::Result<()> {
    let frozen = FrozenValue::from_lua(value).map_err(mlua::Error::external)?;
    let mut state = state.borrow_mut();
    if state.active_module().is_some() || state.root_policy_started {
        return Err(mlua::Error::external(WombatError::configuration(format!(
            "w.target() must run in root policy before module selection at {}",
            location.display()
        ))));
    }
    if let Some(prior) = &state.target_override {
        return Err(mlua::Error::external(WombatError::configuration(format!(
            "w.target() may override the target only once; first at {}, repeated at {}",
            prior.display(),
            location.display()
        ))));
    }
    if let Some(prior) = &state.target_first_read {
        return Err(mlua::Error::external(WombatError::configuration(format!(
            "w.target() cannot override a target after it was read; first read at {}, override at {}",
            prior.display(),
            location.display()
        ))));
    }
    let platform = match frozen {
        FrozenValue::String(value) => {
            TargetPlatform::parse_compact(&value).map_err(mlua::Error::external)?
        }
        value => TargetPlatform::from_frozen(&value).map_err(mlua::Error::external)?,
    };
    state.target = ResolvedTarget {
        platform,
        origin: TargetOrigin::RootOverride,
        declared_at: Some(location.trace.clone()),
    };
    state.target_override = Some(location);
    Ok(())
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
    if !is_module_selection {
        state.root_policy_started = true;
    }
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
        declared_at: location.trace.clone(),
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
            declared_at: location.trace,
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
    requested_kind: &str,
    context: Value,
    location: Location,
) -> Result<()> {
    validate_declared_source(source_path)?;
    if !matches!(requested_kind, "auto" | "file" | "template") {
        return Err(WombatError::configuration(format!(
            "unsupported artifact production kind `{requested_kind}`"
        )));
    }

    let mut state = state.borrow_mut();
    let repository_root = state.root.clone();
    let (source_base, module_anchor) = state.active_location();
    let prefixed = if module_anchor.is_none() {
        prefixed_source(source_path)?
    } else {
        None
    };
    let inference = match (module_anchor, prefixed) {
        (Some(anchor), _) => Some((
            anchor,
            if source_path == "." { "" } else { source_path },
            InferenceBasis::ModuleAnchor,
        )),
        (None, Some((anchor, relative))) => Some((anchor, relative, InferenceBasis::SourcePrefix)),
        (None, None) => None,
    };

    let owner = state.active_module().unwrap_or(ROOT_MODULE).to_string();
    let absolute_source = if source_path == "." {
        source_base
    } else {
        source_base.join(source_path)
    };
    let metadata = match fs::symlink_metadata(&absolute_source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(WombatError::configuration(format!(
                "static artifact source `{}` does not exist or is not a regular file or directory",
                display_path(&repository_root, &absolute_source)
            )));
        }
        Err(error) => return Err(WombatError::io(&absolute_source, error)),
    };
    validate_source_components(&repository_root, &absolute_source)?;
    if metadata.file_type().is_file() {
        let production = match requested_kind {
            "auto" | "file" => EvaluatedProduction::Static,
            "template" => {
                let context = FrozenValue::from_lua(context)?;
                if !matches!(context, FrozenValue::Map(_)) {
                    return Err(WombatError::configuration(
                        "template `with` context must be a string-keyed map",
                    ));
                }
                EvaluatedProduction::Template { context }
            }
            other => {
                return Err(WombatError::configuration(format!(
                    "unsupported artifact production kind `{other}`"
                )));
            }
        };
        let target = match explicit_target {
            Some(target) => parse_explicit_target(target)?,
            None => {
                let (anchor, mut relative, basis) = inference.ok_or_else(|| {
                    WombatError::configuration(format!(
                        "cannot infer a target for source `{source_path}` from an anchorless module; use a `dot_config/`, `dot_local/`, or `home/` source prefix, or provide `to`"
                    ))
                })?;
                if matches!(production, EvaluatedProduction::Template { .. }) {
                    relative = relative.strip_suffix(".tmpl").unwrap_or(relative);
                }
                infer_target(anchor, relative, basis)?
            }
        };
        state.artifacts.push(EvaluatedArtifact {
            kind: ArtifactKind::File,
            source: display_path(&repository_root, &absolute_source),
            source_origin: SourceOrigin::Direct {
                declared: source_path.to_string(),
            },
            production,
            target,
            fingerprint: SourceFingerprint::from_metadata(&metadata),
            owner,
            declared_at: location.trace,
        });
    } else if metadata.file_type().is_dir() {
        if requested_kind == "template" {
            return Err(WombatError::configuration(format!(
                "template source `{}` must be a regular file, not a directory",
                display_path(&repository_root, &absolute_source)
            )));
        }
        if requested_kind == "file" {
            return Err(WombatError::configuration(format!(
                "static file source `{}` must be a regular file, not a directory",
                display_path(&repository_root, &absolute_source)
            )));
        }
        let (anchor, relative_root, basis) = inference.ok_or_else(|| {
            WombatError::configuration(format!(
                "directory source `{source_path}` is outside canonical artifact trees; use `home/`, `dot_config/`, or `dot_local/`"
            ))
        })?;
        let target_root = match explicit_target {
            Some(target) => parse_explicit_target_root(target)?,
            None => infer_target_root(anchor, relative_root, basis)?,
        };
        let snapshot = snapshot_directory(&repository_root, &absolute_source)?;
        let resolved_root = display_path(&repository_root, &absolute_source);
        for leaf in &snapshot {
            let source = join_portable(&absolute_source, &leaf.relative);
            state.artifacts.push(EvaluatedArtifact {
                kind: ArtifactKind::File,
                source: display_path(&repository_root, &source),
                source_origin: SourceOrigin::Directory {
                    declared: source_path.to_string(),
                    root: resolved_root.clone(),
                    relative: leaf.relative.clone(),
                },
                production: EvaluatedProduction::Static,
                target: expand_target_root(&target_root, &leaf.relative)?,
                fingerprint: leaf.fingerprint.clone(),
                owner: owner.clone(),
                declared_at: location.trace.clone(),
            });
        }
        state.directories.push(EvaluatedDirectory {
            declared_source: source_path.to_string(),
            root: resolved_root,
            target_root,
            owner,
            declared_at: location.trace,
            snapshot,
        });
    } else {
        return Err(WombatError::configuration(format!(
            "static artifact source `{}` is not a regular file or directory",
            display_path(&repository_root, &absolute_source)
        )));
    }
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
    let result = load_tracked_source(state, &path).and_then(|source| {
        let value = execute_tracked_chunk(lua, state, &source, &path)?;
        FrozenValue::from_lua(value)
    });
    let selection = state
        .borrow()
        .dependencies
        .iter()
        .find(|dependency| dependency.kind == DependencyKind::Use && dependency.to == name)
        .cloned();
    let result = result.map_err(|error| match &selection {
        Some(selection) => error.with_note(format!(
            "module `{name}` was selected at {}",
            selection.declared_at
        )),
        None => error,
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
        (
            root.join("modules")
                .join("dot_local")
                .join(format!("{name}.lua")),
            root.join("dot_local"),
            Some(SourceAnchor::DotLocal),
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
            source: module
                .location
                .as_ref()
                .map(|location| display_path(&state.root, &location.file))
                .expect("selected modules are evaluated before manifest construction"),
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
            .then_with(|| left.declared_at.cmp(&right.declared_at))
    });
    let mut directories = state.directories.clone();
    directories.sort_by(|left, right| {
        left.root
            .cmp(&right.root)
            .then_with(|| left.owner.cmp(&right.owner))
            .then_with(|| left.declared_at.cmp(&right.declared_at))
    });

    EvaluatedManifest {
        sources: state
            .sources
            .values()
            .map(|source| source.manifest.clone())
            .collect(),
        inputs: state.inputs.clone(),
        target: state.target.clone(),
        observations: state.observations.values().cloned().collect(),
        modules,
        dependencies,
        artifacts,
        directories,
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
            let source = match &artifact.source_origin {
                SourceOrigin::Direct { declared } => {
                    format!("`{}` (direct source `{declared}`)", artifact.source)
                }
                SourceOrigin::Directory {
                    declared,
                    root,
                    relative,
                } => format!(
                    "`{}` (leaf `{relative}` expanded from directory `{declared}` at `{root}`)",
                    artifact.source
                ),
            };
            format!(
                "{} from {source} declared at {}",
                artifact.owner, artifact.declared_at
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    WombatError::configuration(format!(
        "artifact conflict at `{target}`: {reason}; declarations: {declarations}"
    ))
}

fn execute_tracked_chunk(
    lua: &Lua,
    state: &Rc<RefCell<RuntimeState>>,
    source: &str,
    path: &Path,
) -> Result<Value> {
    state.borrow_mut().failure_frames.clear();
    state.borrow_mut().failure_tail_call = false;
    let chunk = lua
        .load(source)
        .set_name(format!("@{}", path.to_string_lossy()))
        .into_function()
        .map_err(|error| lua_diagnostic(state, error, Some(path)))?;
    let handler_state = Rc::clone(state);
    let handler = lua.create_function(move |lua, error: Value| {
        let (frames, tail_call) = capture_user_frames(lua, &handler_state);
        let mut state = handler_state.borrow_mut();
        state.failure_frames = frames;
        state.failure_tail_call = tail_call;
        Ok(error)
    })?;
    let protected: Function = lua
        .load(
            "return function(chunk, handler)\n\
             local result = table.pack(xpcall(chunk, handler))\n\
             if not result[1] then error(result[2], 0) end\n\
             return table.unpack(result, 2, result.n)\n\
             end",
        )
        .set_name("=<wombat>/protected.lua")
        .eval()?;
    protected
        .call((chunk, handler))
        .map_err(|error| lua_diagnostic(state, error, Some(path)))
}

fn lua_diagnostic(
    state: &Rc<RefCell<RuntimeState>>,
    error: mlua::Error,
    fallback_path: Option<&Path>,
) -> WombatError {
    let state = state.borrow();
    let mut frames = state.failure_frames.clone();
    if frames.is_empty()
        && let Some(path) = fallback_path
    {
        frames.push(SourceLocation {
            source: display_path(&state.root, path),
            line: syntax_line(&error),
            column: None,
        });
    }
    let primary = frames.first().cloned();
    let source_line = primary.as_ref().and_then(|location| {
        let line = usize::try_from(location.line?).ok()?;
        state
            .sources
            .get(&location.source)?
            .snapshot
            .lines()
            .nth(line.saturating_sub(1))
            .map(str::to_string)
    });
    let raw = error.to_string();
    let mut diagnostic = Diagnostic::new(clean_lua_reason(&raw));
    diagnostic.primary = primary;
    diagnostic.source_line = source_line;
    diagnostic.user_frames = frames;
    if let (Some(primary), Some(caller)) = (
        diagnostic.user_frames.first(),
        diagnostic.user_frames.get(1),
    ) && primary.source != caller.source
    {
        diagnostic.notes.push(format!("called from {caller}"));
    }
    if state.failure_tail_call {
        diagnostic.notes.push(
            "Lua reported a tail call; intermediate user frames may be unavailable".to_string(),
        );
    }
    diagnostic.underlying = Some(raw);
    WombatError::diagnostic(diagnostic)
}

fn syntax_line(error: &mlua::Error) -> Option<u32> {
    let message = match error {
        mlua::Error::SyntaxError { message, .. } => message,
        _ => return None,
    };
    parse_lua_line(message)
}

fn parse_lua_line(message: &str) -> Option<u32> {
    message.split(':').find_map(|part| part.parse::<u32>().ok())
}

fn clean_lua_reason(raw: &str) -> String {
    let first = raw.split("\nstack traceback:").next().unwrap_or(raw);
    let bytes = first.as_bytes();
    for start in 0..bytes.len() {
        if bytes[start] != b':' {
            continue;
        }
        let digits_start = start + 1;
        let mut end = digits_start;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if end > digits_start && end < bytes.len() && bytes[end] == b':' {
            return first[end + 1..].trim().to_string();
        }
    }
    first
        .trim()
        .strip_prefix("runtime error:")
        .or_else(|| first.trim().strip_prefix("syntax error:"))
        .unwrap_or(first.trim())
        .trim()
        .to_string()
}

fn capture_user_frames(
    lua: &Lua,
    state: &Rc<RefCell<RuntimeState>>,
) -> (Vec<SourceLocation>, bool) {
    let root = state.borrow().root.clone();
    let mut frames = Vec::new();
    let mut tail_call = false;
    for level in 1..=48 {
        if frames.len() == MAX_SOURCE_TRACE_FRAMES {
            break;
        }
        let frame = lua
            .inspect_stack(level, |debug| {
                let source = debug.source().source?.into_owned();
                if source == "<wombat>/init.lua"
                    || source == "=<wombat>/init.lua"
                    || source == "<wombat>/protected.lua"
                    || source == "=<wombat>/protected.lua"
                    || source == "=[C]"
                    || source == "[C]"
                    || source == "<unknown>"
                {
                    return None;
                }
                let source = source.strip_prefix('@').unwrap_or(&source);
                Some((
                    SourceLocation {
                        source: display_path(&root, Path::new(source)),
                        line: debug
                            .current_line()
                            .and_then(|line| u32::try_from(line).ok()),
                        column: None,
                    },
                    debug.is_tail_call(),
                ))
            })
            .flatten();
        let Some((frame, is_tail_call)) = frame else {
            continue;
        };
        tail_call |= is_tail_call;
        if frames.last() != Some(&frame) {
            frames.push(frame);
        }
    }
    (frames, tail_call)
}

fn caller_location(lua: &Lua, state: &Rc<RefCell<RuntimeState>>) -> Location {
    let (frames, _) = capture_user_frames(lua, state);
    let primary = frames.first().cloned().unwrap_or(SourceLocation {
        source: "<unknown>".to_string(),
        line: None,
        column: None,
    });
    Location {
        trace: SourceTrace {
            primary,
            callers: frames.into_iter().skip(1).collect(),
        },
    }
}

fn display_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn load_tracked_source(state: &Rc<RefCell<RuntimeState>>, path: &Path) -> Result<String> {
    let root = state.borrow().root.clone();
    let relative = path.strip_prefix(&root).map_err(|_| {
        WombatError::configuration(format!(
            "Lua source `{}` escapes the repository",
            path.display()
        ))
    })?;
    let mut current = root.clone();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return Err(WombatError::configuration(format!(
                "Lua source `{}` contains an invalid path component",
                path.display()
            )));
        };
        current.push(component);
        let metadata =
            fs::symlink_metadata(&current).map_err(|error| WombatError::io(&current, error))?;
        if metadata.file_type().is_symlink() {
            return Err(WombatError::configuration(format!(
                "Lua source `{}` must not contain symbolic links",
                path.display()
            )));
        }
    }
    let before = fs::symlink_metadata(path).map_err(|error| WombatError::io(path, error))?;
    if !before.file_type().is_file() {
        return Err(WombatError::configuration(format!(
            "Lua source `{}` is not a regular file",
            path.display()
        )));
    }
    let bytes = fs::read(path).map_err(|error| WombatError::io(path, error))?;
    let after = fs::symlink_metadata(path).map_err(|error| WombatError::io(path, error))?;
    let before_fingerprint = SourceFingerprint::from_metadata(&before);
    if SourceFingerprint::from_metadata(&after) != before_fingerprint {
        return Err(WombatError::configuration(format!(
            "Lua source `{}` changed while it was being read",
            path.display()
        )));
    }
    let snapshot = String::from_utf8(bytes.clone()).map_err(|_| {
        WombatError::configuration(format!(
            "Lua source `{}` is not valid UTF-8",
            path.display()
        ))
    })?;
    let portable = display_path(&root, path);
    let manifest = SourceFile {
        path: portable.clone(),
        digest: digest_bytes(&bytes),
    };
    let mut state = state.borrow_mut();
    if let Some(existing) = state.sources.get(&portable) {
        if existing.manifest != manifest || existing.fingerprint != before_fingerprint {
            return Err(WombatError::configuration(format!(
                "Lua source `{portable}` changed during evaluation"
            )));
        }
        return Ok(existing.snapshot.clone());
    }
    state.sources.insert(
        portable,
        TrackedSource {
            manifest,
            fingerprint: before_fingerprint,
            snapshot: snapshot.clone(),
        },
    );
    Ok(snapshot)
}

fn digest_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(7 + digest.len() * 2);
    output.push_str("sha256:");
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to a string cannot fail");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::{helper_module_path, is_path_ancestor, validate_module_name};

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

    #[test]
    fn normalizes_safe_repository_helper_names() {
        assert_eq!(helper_module_path("theme.colors").unwrap(), "theme/colors");
        assert!(helper_module_path("../theme").is_err());
        assert!(helper_module_path("theme/path").is_err());
    }
}
