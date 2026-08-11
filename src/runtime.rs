use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use mlua::{Function, Lua, LuaOptions, MultiValue, StdLib, Table, Value};
use sha2::{Digest, Sha256};

use crate::context::{
    HostContext, OperatingSystemName, ResolvedTarget, TargetOrigin, TargetPlatform,
};
use crate::frozen::FrozenValue;
use crate::inputs::{self, InputSpec};
use crate::manifest::{
    ArtifactKind, BuildInput, Dependency, DependencyKind, EvaluatedArtifact, EvaluatedDirectory,
    EvaluatedManifest, EvaluatedProduction, EvaluatedTask, InferenceBasis, MAX_SOURCE_TRACE_FRAMES,
    ManifestModule, Observation, ObservationSubject, Provider, ProviderBinding, ProviderOrigin,
    ProviderPreparation, Publications, Requirement, RequirementCandidate, RequirementChoice,
    RequirementKind, ResolutionAttempt, ResolutionOutcome, SourceAnchor, SourceFile,
    SourceLocation, SourceOrigin, SourceTrace, Task, TaskCachePolicy, TaskLogPolicy, TaskRunner,
    TaskRunnerFamily, TaskTargetRoot,
};
use crate::path::{
    expand_target_root, infer_target, infer_target_root, parse_explicit_target,
    parse_explicit_target_root, prefixed_source, reject_noncanonical_artifact_trees,
    validate_declared_source, validate_relative_path,
};
use crate::source::{
    SourceFingerprint, fingerprint_regular_file, join_portable, snapshot_directory,
    validate_source_components,
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
    providers: Vec<Provider>,
    requirements: Vec<Requirement>,
    build_providers: Vec<Provider>,
    build_providers_declared: bool,
    build_requirements: Vec<Requirement>,
    task_interpreters: BTreeMap<String, TaskRunner>,
    tasks: Vec<EvaluatedTask>,
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
    pub task_interpreters: BTreeMap<String, TaskRunner>,
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
            task_interpreters: BTreeMap::new(),
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
        providers: Vec::new(),
        requirements: Vec::new(),
        build_providers: Vec::new(),
        build_providers_declared: false,
        build_requirements: Vec::new(),
        task_interpreters: options.task_interpreters,
        tasks: Vec::new(),
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
    let preparations = plan_provider_preparations(&state, false)?;
    let build_preparations = plan_provider_preparations(&state, true)?;

    Ok(EvaluationOutcome::Manifest(Box::new(build_manifest(
        &state.borrow(),
        build_preparations,
        preparations,
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

    let providers_state = Rc::clone(&state);
    native.set(
        "configure_providers",
        lua.create_function(move |lua, entries: Value| {
            let location = caller_location(lua, &providers_state);
            configure_providers(&providers_state, entries, location, false)
                .map_err(mlua::Error::external)
        })?,
    )?;

    let build_providers_state = Rc::clone(&state);
    native.set(
        "configure_build_providers",
        lua.create_function(move |lua, entries: Value| {
            let location = caller_location(lua, &build_providers_state);
            configure_providers(&build_providers_state, entries, location, true)
                .map_err(mlua::Error::external)
        })?,
    )?;

    let requirement_state = Rc::clone(&state);
    native.set(
        "declare_requirement",
        lua.create_function(
            move |lua, (kind, name, options, preferred): (String, String, Value, bool)| {
                let location = caller_location(lua, &requirement_state);
                declare_requirement(
                    lua,
                    &requirement_state,
                    RequirementDeclaration {
                        kind: &kind,
                        name: &name,
                        options,
                        preferred,
                        location,
                        build_scope: false,
                    },
                )
                .map_err(mlua::Error::external)
            },
        )?,
    )?;

    let build_requirement_state = Rc::clone(&state);
    native.set(
        "declare_build_requirement",
        lua.create_function(
            move |lua, (kind, name, options, preferred): (String, String, Value, bool)| {
                let location = caller_location(lua, &build_requirement_state);
                declare_requirement(
                    lua,
                    &build_requirement_state,
                    RequirementDeclaration {
                        kind: &kind,
                        name: &name,
                        options,
                        preferred,
                        location,
                        build_scope: true,
                    },
                )
                .map_err(mlua::Error::external)
            },
        )?,
    )?;

    let task_state = Rc::clone(&state);
    native.set(
        "declare_task",
        lua.create_function(
            move |lua, (entrypoint, params, options): (String, Value, Value)| {
                let location = caller_location(lua, &task_state);
                declare_task(lua, &task_state, &entrypoint, params, options, location)
                    .map_err(mlua::Error::external)
            },
        )?,
    )?;

    let generated_state = Rc::clone(&state);
    native.set(
        "declare_generated",
        lua.create_function(move |lua, (name, options): (String, Value)| {
            let location = caller_location(lua, &generated_state);
            declare_generated(&generated_state, &name, options, location)
                .map_err(mlua::Error::external)
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
        FrozenValue::Map(values) => {
            let proxy = lua.create_table()?;
            let metatable = lua.create_table()?;
            metatable.set(
                "__index",
                lua.create_function(move |lua, (_table, key): (Table, String)| {
                    values
                        .get(&key)
                        .cloned()
                        .map_or(Ok(Value::Nil), |value| readonly_frozen(lua, value))
                })?,
            )?;
            metatable.set(
                "__newindex",
                lua.create_function(|_, (_table, _key, _value): (Table, Value, Value)| {
                    Err::<(), _>(mlua::Error::external(WombatError::configuration(
                        "resolved Wombat values are immutable",
                    )))
                })?,
            )?;
            metatable.set("__metatable", false)?;
            proxy.set_metatable(Some(metatable))?;
            Ok(Value::Table(proxy))
        }
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

fn configure_providers(
    state: &Rc<RefCell<RuntimeState>>,
    entries: Value,
    location: Location,
    build_scope: bool,
) -> Result<()> {
    let frozen = FrozenValue::from_lua(entries)?;
    let values = match frozen {
        FrozenValue::Array(values) => values,
        FrozenValue::Map(values) if values.is_empty() => Vec::new(),
        _ => {
            return Err(WombatError::configuration(
                "w.providers() requires an array of provider names or provider option tables",
            ));
        }
    };
    let mut configured = Vec::with_capacity(values.len());
    let root = {
        let mut state = state.borrow_mut();
        if state.active_module().is_some() {
            return Err(WombatError::configuration(format!(
                "{} belongs to root policy at {}",
                if build_scope {
                    "w.build.providers()"
                } else {
                    "w.providers()"
                },
                location.display()
            )));
        }
        let already_declared = if build_scope {
            state.build_providers_declared
        } else {
            !state.providers.is_empty()
        };
        if already_declared {
            return Err(WombatError::configuration(format!(
                "{} may be declared only once; repeated at {}",
                if build_scope {
                    "w.build.providers()"
                } else {
                    "w.providers()"
                },
                location.display()
            )));
        }
        if !state.modules.is_empty()
            || !state.artifacts.is_empty()
            || !state.requirements.is_empty()
            || !state.build_requirements.is_empty()
            || !state.tasks.is_empty()
        {
            return Err(WombatError::configuration(format!(
                "{} must run before use(), using(), install(), need(), generate(), or build.task() at {}",
                if build_scope {
                    "w.build.providers()"
                } else {
                    "w.providers()"
                },
                location.display()
            )));
        }
        state.root_policy_started = true;
        if build_scope {
            state.build_providers_declared = true;
        }
        state.root.clone()
    };

    let mut names = BTreeSet::new();
    for (index, value) in values.into_iter().enumerate() {
        let (name, config) = match value {
            FrozenValue::String(name) => (name, FrozenValue::empty_map()),
            FrozenValue::Map(mut options) => {
                let name = take_string(&mut options, "name", "provider")?;
                let config = options
                    .remove("with")
                    .unwrap_or_else(FrozenValue::empty_map);
                if !matches!(config, FrozenValue::Map(_)) {
                    return Err(WombatError::configuration(format!(
                        "provider `{name}` requires map-shaped `with` options"
                    )));
                }
                reject_unknown_options(&options, "provider")?;
                (name, config)
            }
            _ => {
                return Err(WombatError::configuration(format!(
                    "provider entry {} must be a string or table",
                    index + 1
                )));
            }
        };
        validate_provider_name(&name)?;
        if !names.insert(name.clone()) {
            return Err(WombatError::configuration(format!(
                "provider `{name}` is configured more than once"
            )));
        }
        let origin = if matches!(name.as_str(), "brew" | "apt") {
            let conflicting = root.join("providers").join(format!("{name}.lua"));
            if conflicting.exists() {
                return Err(WombatError::configuration(format!(
                    "custom provider `providers/{name}.lua` conflicts with reserved built-in provider `{name}`"
                )));
            }
            ProviderOrigin::Builtin {
                contract_version: 1,
            }
        } else {
            let entrypoint = root.join("providers").join(format!("{name}.lua"));
            let snapshot = load_tracked_source(state, &entrypoint).map_err(|error| {
                error.with_note(format!(
                    "custom provider `{name}` must be defined at `providers/{name}.lua`"
                ))
            })?;
            let source_path = format!("providers/{name}.lua");
            let digest = state
                .borrow()
                .sources
                .get(&source_path)
                .expect("loaded provider source must be tracked")
                .manifest
                .digest
                .clone();
            ProviderOrigin::Custom {
                entrypoint: format!("{name}.lua"),
                files: vec![crate::manifest::ProviderFile {
                    source: source_path,
                    payload: format!("{name}.lua"),
                    digest,
                    size: u64::try_from(snapshot.len())
                        .map_err(|_| WombatError::configuration("provider source is too large"))?,
                }],
            }
        };
        configured.push(Provider {
            name,
            priority: u32::try_from(index)
                .map_err(|_| WombatError::configuration("too many configured providers"))?,
            config,
            origin,
            declared_at: location.trace.clone(),
        });
    }
    let custom_names = configured
        .iter()
        .filter(|provider| matches!(provider.origin, ProviderOrigin::Custom { .. }))
        .map(|provider| provider.name.clone())
        .collect::<Vec<_>>();
    if build_scope {
        state.borrow_mut().build_providers = configured;
    } else {
        state.borrow_mut().providers = configured;
    }
    for name in custom_names {
        validate_custom_provider(state, &name)?;
        record_provider_sources(state, &name)?;
    }
    Ok(())
}

struct RequirementDeclaration<'a> {
    kind: &'a str,
    name: &'a str,
    options: Value,
    preferred: bool,
    location: Location,
    build_scope: bool,
}

fn declare_requirement(
    lua: &Lua,
    state: &Rc<RefCell<RuntimeState>>,
    declaration: RequirementDeclaration<'_>,
) -> Result<Value> {
    let RequirementDeclaration {
        kind,
        name,
        options,
        preferred,
        location,
        build_scope,
    } = declaration;
    let options = if options.is_nil() {
        FrozenValue::empty_map()
    } else {
        FrozenValue::from_lua(options)?
    };
    let FrozenValue::Map(mut options) = options else {
        return Err(WombatError::configuration(format!(
            "w.{}{}.{}() options must be a table",
            if build_scope { "build." } else { "" },
            if preferred { "prefer" } else { "need" },
            kind
        )));
    };
    let requirement_kind = match kind {
        "command" => RequirementKind::Command,
        "package" => RequirementKind::Package,
        _ => {
            return Err(WombatError::configuration(format!(
                "unknown requirement kind `{kind}`"
            )));
        }
    };
    let mut candidates = vec![parse_requirement_candidate(
        requirement_kind,
        name,
        &mut options,
        false,
    )?];
    if let Some(accept) = options.remove("accept") {
        if !preferred {
            return Err(WombatError::configuration(
                "w.need() does not support `accept`; use w.prefer()",
            ));
        }
        let values = match accept {
            FrozenValue::String(name) if requirement_kind == RequirementKind::Command => {
                vec![FrozenValue::String(name)]
            }
            FrozenValue::Map(value) => vec![FrozenValue::Map(value)],
            FrozenValue::Array(values) => values,
            _ => {
                return Err(WombatError::configuration(
                    "prefer `accept` must be a command string, candidate table, or candidate array",
                ));
            }
        };
        if values.is_empty() {
            return Err(WombatError::configuration(
                "prefer `accept` must contain at least one candidate",
            ));
        }
        for value in values {
            match value {
                FrozenValue::String(name) if requirement_kind == RequirementKind::Command => {
                    let mut empty = BTreeMap::new();
                    candidates.push(parse_requirement_candidate(
                        requirement_kind,
                        &name,
                        &mut empty,
                        true,
                    )?);
                }
                FrozenValue::Map(mut candidate) => {
                    let candidate_name = take_string(&mut candidate, "name", "accepted candidate")?;
                    candidates.push(parse_requirement_candidate(
                        requirement_kind,
                        &candidate_name,
                        &mut candidate,
                        true,
                    )?);
                }
                _ => {
                    return Err(WombatError::configuration(
                        "accepted command candidates must be strings or tables and package candidates must be tables",
                    ));
                }
            }
        }
    }
    reject_unknown_options(&options, "requirement")?;

    ensure_build_provider_defaults(state, &location, build_scope)?;
    let (owner, providers, target) = {
        let mut state = state.borrow_mut();
        let providers = if build_scope {
            state.build_providers.clone()
        } else {
            state.providers.clone()
        };
        if providers.is_empty() {
            return Err(WombatError::configuration(format!(
                "{} requirements need provider policy; call {} before {} at {}",
                if build_scope { "build" } else { "target" },
                if build_scope {
                    "w.build.providers()"
                } else {
                    "w.providers()"
                },
                if preferred { "w.prefer()" } else { "w.need()" },
                location.display()
            )));
        }
        if state.active_module().is_none() {
            state.root_policy_started = true;
        }
        (
            state.active_module().unwrap_or(ROOT_MODULE).to_string(),
            providers,
            if build_scope {
                state.host.platform.clone()
            } else {
                effective_target(&state)
            },
        )
    };

    let mut attempts = Vec::new();
    let mut selected = None;
    for (candidate_index, candidate) in candidates.iter().enumerate() {
        if let RequirementCandidate::Package {
            provider: required, ..
        } = candidate
            && !providers.iter().any(|provider| provider.name == *required)
        {
            return Err(WombatError::configuration(format!(
                "package candidate `{}` requests provider `{required}`, which is not configured",
                candidate.name()
            )));
        }
        let candidates_providers = providers.iter().filter(|provider| match candidate {
            RequirementCandidate::Command { .. } => true,
            RequirementCandidate::Package {
                provider: required, ..
            } => provider.name == *required,
        });
        for provider in candidates_providers {
            let outcome = resolve_provider_requirement(state, provider, candidate, &target)?;
            let candidate_index = u32::try_from(candidate_index)
                .map_err(|_| WombatError::configuration("too many requirement candidates"))?;
            match outcome {
                Ok(binding) => {
                    attempts.push(ResolutionAttempt {
                        candidate: candidate_index,
                        provider: provider.name.clone(),
                        outcome: ResolutionOutcome::Selected,
                    });
                    selected = Some((candidate_index, binding));
                    break;
                }
                Err(reason) => attempts.push(ResolutionAttempt {
                    candidate: candidate_index,
                    provider: provider.name.clone(),
                    outcome: ResolutionOutcome::Unsupported { reason },
                }),
            }
        }
        if selected.is_some() {
            break;
        }
    }
    let Some((selected_index, binding)) = selected else {
        let reasons = attempts
            .iter()
            .map(|attempt| match &attempt.outcome {
                ResolutionOutcome::Unsupported { reason } => format!(
                    "candidate {} through `{}`: {reason}",
                    attempt.candidate + 1,
                    attempt.provider
                ),
                ResolutionOutcome::Selected => unreachable!(),
            })
            .collect::<Vec<_>>()
            .join("; ");
        return Err(WombatError::configuration(format!(
            "no configured provider can resolve {kind} requirement `{name}` at {}: {reasons}",
            location.display()
        )));
    };
    let choice = if !preferred {
        RequirementChoice::Required
    } else if selected_index == 0 {
        RequirementChoice::Preferred
    } else {
        RequirementChoice::Accepted
    };
    let requirement = Requirement {
        kind: requirement_kind,
        owner,
        declared_at: location.trace,
        candidates,
        attempts,
        selected: selected_index,
        choice,
        binding,
    };
    let handle = resolved_requirement_handle(&requirement);
    if build_scope {
        state.borrow_mut().build_requirements.push(requirement);
    } else {
        state.borrow_mut().requirements.push(requirement);
    }
    readonly_frozen(lua, handle).map_err(WombatError::from)
}

fn ensure_build_provider_defaults(
    state: &Rc<RefCell<RuntimeState>>,
    location: &Location,
    build_scope: bool,
) -> Result<()> {
    if !build_scope {
        return Ok(());
    }
    let mut state = state.borrow_mut();
    if state.build_providers_declared || !state.build_providers.is_empty() {
        return Ok(());
    }
    let (name, config) = match state.host.platform.os.name {
        OperatingSystemName::Macos => ("brew", FrozenValue::empty_map()),
        OperatingSystemName::Linux => {
            let debian = state
                .host
                .platform
                .os
                .distribution
                .as_ref()
                .is_some_and(|distribution| {
                    matches!(distribution.id.as_str(), "debian" | "ubuntu")
                        || distribution.id_like.iter().any(|id| id == "debian")
                });
            if !debian {
                return Err(WombatError::configuration(format!(
                    "cannot infer a host build provider for Linux distribution at {}; call w.build.providers() explicitly",
                    location.display()
                )));
            }
            (
                "apt",
                FrozenValue::Map(BTreeMap::from([(
                    "update".to_string(),
                    FrozenValue::Boolean(true),
                )])),
            )
        }
    };
    state.build_providers.push(Provider {
        name: name.to_string(),
        priority: 0,
        config,
        origin: ProviderOrigin::Builtin {
            contract_version: 1,
        },
        declared_at: location.trace.clone(),
    });
    Ok(())
}

fn parse_requirement_candidate(
    kind: RequirementKind,
    name: &str,
    options: &mut BTreeMap<String, FrozenValue>,
    accepted: bool,
) -> Result<RequirementCandidate> {
    validate_product_name(name, kind)?;
    let minimum = take_optional_string(options, "minimum", "requirement")?;
    if minimum.as_deref().is_some_and(str::is_empty) {
        return Err(WombatError::configuration(
            "requirement minimum version must not be empty",
        ));
    }
    match kind {
        RequirementKind::Command => {
            if accepted {
                reject_unknown_options(options, "accepted command candidate")?;
            }
            Ok(RequirementCandidate::Command {
                name: name.to_string(),
                minimum,
            })
        }
        RequirementKind::Package => {
            let provider = take_string(options, "provider", "package requirement")?;
            validate_provider_name(&provider)?;
            let publications = options
                .remove("publishes")
                .map(parse_publications)
                .transpose()?
                .unwrap_or(Publications {
                    commands: Vec::new(),
                });
            let with = options
                .remove("with")
                .unwrap_or_else(FrozenValue::empty_map);
            if !matches!(with, FrozenValue::Map(_)) {
                return Err(WombatError::configuration(
                    "package requirement `with` must be a string-keyed map",
                ));
            }
            if accepted {
                reject_unknown_options(options, "accepted package candidate")?;
            }
            Ok(RequirementCandidate::Package {
                name: name.to_string(),
                provider,
                minimum,
                publications,
                with,
            })
        }
    }
}

fn parse_publications(value: FrozenValue) -> Result<Publications> {
    let FrozenValue::Map(mut values) = value else {
        return Err(WombatError::configuration(
            "package `publishes` must be a table",
        ));
    };
    let commands = match values.remove("commands") {
        None => Vec::new(),
        Some(FrozenValue::Array(values)) => values
            .into_iter()
            .map(|value| match value {
                FrozenValue::String(command) => {
                    validate_product_name(&command, RequirementKind::Command)?;
                    Ok(command)
                }
                _ => Err(WombatError::configuration(
                    "published commands must be strings",
                )),
            })
            .collect::<Result<Vec<_>>>()?,
        Some(_) => {
            return Err(WombatError::configuration(
                "package `publishes.commands` must be an array",
            ));
        }
    };
    reject_unknown_options(&values, "package publications")?;
    let mut sorted = commands;
    sorted.sort();
    if sorted.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(WombatError::configuration(
            "published commands must be unique",
        ));
    }
    Ok(Publications { commands: sorted })
}

fn resolve_provider_requirement(
    state: &Rc<RefCell<RuntimeState>>,
    provider: &Provider,
    candidate: &RequirementCandidate,
    target: &TargetPlatform,
) -> Result<std::result::Result<ProviderBinding, String>> {
    let lua = Lua::new_with(
        StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8,
        LuaOptions::default(),
    )?;
    for name in ["dofile", "load", "loadfile"] {
        lua.globals().set(name, Value::Nil)?;
    }
    let api = provider_api(&lua)?;
    let source = match &provider.origin {
        ProviderOrigin::Builtin { .. } => builtin_provider_source(&provider.name)?.to_string(),
        ProviderOrigin::Custom { entrypoint, .. } => {
            install_provider_require(&lua, state.clone(), &provider.name, api.clone())?;
            let path = state.borrow().root.join("providers").join(entrypoint);
            load_tracked_source(state, &path)?
        }
    };
    if matches!(provider.origin, ProviderOrigin::Builtin { .. }) {
        let api_for_require = api.clone();
        lua.globals().set(
            "require",
            lua.create_function(move |_, name: String| {
                if name == "wombat.provider" {
                    Ok(api_for_require.clone())
                } else {
                    Err(mlua::Error::external(WombatError::configuration(format!(
                        "built-in provider cannot require `{name}`"
                    ))))
                }
            })?,
        )?;
    }
    let definition: Table = lua
        .load(&source)
        .set_name(format!("@providers/{}.lua", provider.name))
        .eval()
        .map_err(|error| provider_lua_error(&provider.name, "load", error))?;
    let resolve: Function = definition.get("resolve").map_err(|error| {
        provider_lua_error(&provider.name, "definition requires resolve", error)
    })?;
    let candidate = frozen_candidate(candidate)?;
    let result: Value = resolve
        .call((
            candidate.to_lua(&lua)?,
            target.to_frozen().to_lua(&lua)?,
            provider.config.to_lua(&lua)?,
        ))
        .map_err(|error| provider_lua_error(&provider.name, "resolve", error))?;
    let frozen = FrozenValue::from_lua(result)?;
    record_provider_sources(state, &provider.name)?;
    parse_provider_resolution(&provider.name, frozen)
}

const BREW_PROVIDER_LUA: &str = include_str!("../lua/wombat/providers/brew.lua");
const APT_PROVIDER_LUA: &str = include_str!("../lua/wombat/providers/apt.lua");

fn builtin_provider_source(name: &str) -> Result<&'static str> {
    match name {
        "brew" => Ok(BREW_PROVIDER_LUA),
        "apt" => Ok(APT_PROVIDER_LUA),
        _ => Err(WombatError::configuration(format!(
            "unknown built-in provider `{name}`"
        ))),
    }
}

fn provider_api(lua: &Lua) -> Result<Table> {
    let api = lua.create_table()?;
    api.set(
        "define",
        lua.create_function(|_, definition: Table| Ok(definition))?,
    )?;
    api.set(
        "unsupported",
        lua.create_function(|lua, reason: String| {
            let result = lua.create_table()?;
            result.set("kind", "unsupported")?;
            result.set("reason", reason)?;
            Ok(result)
        })?,
    )?;
    api.set(
        "binding",
        lua.create_function(|_, binding: Table| {
            binding.set("kind", "binding")?;
            Ok(binding)
        })?,
    )?;
    api.set(
        "operation",
        lua.create_function(|_, operation: Table| {
            operation.set("kind", "operation")?;
            Ok(operation)
        })?,
    )?;
    Ok(api)
}

fn validate_custom_provider(state: &Rc<RefCell<RuntimeState>>, name: &str) -> Result<()> {
    let lua = Lua::new_with(
        StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8,
        LuaOptions::default(),
    )?;
    for global in ["dofile", "load", "loadfile"] {
        lua.globals().set(global, Value::Nil)?;
    }
    let api = provider_api(&lua)?;
    install_provider_require(&lua, state.clone(), name, api)?;
    let entrypoint = state
        .borrow()
        .root
        .join("providers")
        .join(format!("{name}.lua"));
    let source = load_tracked_source(state, &entrypoint)?;
    let definition: Table = lua
        .load(&source)
        .set_name(format!("@providers/{name}.lua"))
        .eval()
        .map_err(|error| provider_lua_error(name, "load", error))?;
    for operation in ["resolve", "check", "reconcile"] {
        definition.get::<Function>(operation).map_err(|error| {
            provider_lua_error(name, &format!("definition requires {operation}()"), error)
        })?;
    }
    if definition.get::<Option<Function>>("plan")?.is_some()
        && definition.get::<Option<Function>>("prepare")?.is_none()
    {
        return Err(WombatError::configuration(format!(
            "provider `{name}` definition with plan() requires prepare()"
        )));
    }
    Ok(())
}

fn plan_provider_preparations(
    state: &Rc<RefCell<RuntimeState>>,
    build_scope: bool,
) -> Result<Vec<ProviderPreparation>> {
    let (providers, requirements, target) = {
        let state = state.borrow();
        (
            if build_scope {
                state.build_providers.clone()
            } else {
                state.providers.clone()
            },
            if build_scope {
                state.build_requirements.clone()
            } else {
                state.requirements.clone()
            },
            if build_scope {
                state.host.platform.clone()
            } else {
                effective_target(&state)
            },
        )
    };
    let mut preparations = Vec::new();
    for provider in providers {
        let lua = Lua::new_with(
            StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8,
            LuaOptions::default(),
        )?;
        for name in ["dofile", "load", "loadfile"] {
            lua.globals().set(name, Value::Nil)?;
        }
        let api = provider_api(&lua)?;
        let source = match &provider.origin {
            ProviderOrigin::Builtin { .. } => builtin_provider_source(&provider.name)?.to_string(),
            ProviderOrigin::Custom { entrypoint, .. } => {
                install_provider_require(&lua, Rc::clone(state), &provider.name, api.clone())?;
                let path = state.borrow().root.join("providers").join(entrypoint);
                load_tracked_source(state, &path)?
            }
        };
        if matches!(provider.origin, ProviderOrigin::Builtin { .. }) {
            let api_for_require = api.clone();
            lua.globals().set(
                "require",
                lua.create_function(move |_, name: String| {
                    if name == "wombat.provider" {
                        Ok(api_for_require.clone())
                    } else {
                        Err(mlua::Error::external(WombatError::configuration(format!(
                            "built-in provider cannot require `{name}`"
                        ))))
                    }
                })?,
            )?;
        }
        let definition: Table = lua
            .load(&source)
            .set_name(format!("@providers/{}.lua", provider.name))
            .eval()
            .map_err(|error| provider_lua_error(&provider.name, "load", error))?;
        let Some(plan) = definition
            .get::<Option<Function>>("plan")
            .map_err(|error| provider_lua_error(&provider.name, "plan lookup", error))?
        else {
            continue;
        };
        let mut binding_values = Vec::new();
        for requirement in requirements
            .iter()
            .filter(|requirement| requirement.binding.provider == provider.name)
        {
            binding_values.push(frozen_binding(&requirement.binding)?.to_lua(&lua)?);
        }
        let bindings = lua.create_sequence_from(binding_values)?;
        let value: Value = plan
            .call((
                bindings,
                target.to_frozen().to_lua(&lua)?,
                provider.config.to_lua(&lua)?,
            ))
            .map_err(|error| provider_lua_error(&provider.name, "plan", error))?;
        let operations = match FrozenValue::from_lua(value)? {
            FrozenValue::Array(operations) => operations,
            FrozenValue::Map(values) if values.is_empty() => Vec::new(),
            _ => {
                return Err(WombatError::configuration(format!(
                    "provider `{}` plan() must return an array of provider.operation() values",
                    provider.name
                )));
            }
        };
        for operation in operations {
            preparations.push(parse_provider_operation(&provider.name, operation)?);
        }
        record_provider_sources(state, &provider.name)?;
    }
    let mut identities = BTreeSet::new();
    for preparation in &preparations {
        if !identities.insert((preparation.provider.as_str(), preparation.identity.as_str())) {
            return Err(WombatError::configuration(format!(
                "provider `{}` planned duplicate operation `{}`",
                preparation.provider, preparation.identity
            )));
        }
    }
    Ok(preparations)
}

fn parse_provider_operation(provider: &str, value: FrozenValue) -> Result<ProviderPreparation> {
    let FrozenValue::Map(mut values) = value else {
        return Err(WombatError::configuration(format!(
            "provider `{provider}` plan() entries must be provider.operation() values"
        )));
    };
    let kind = take_string(&mut values, "kind", "provider operation")?;
    if kind != "operation" {
        return Err(WombatError::configuration(format!(
            "provider `{provider}` returned unknown planned value `{kind}`"
        )));
    }
    let identity = take_string(&mut values, "identity", "provider operation")?;
    let description = take_string(&mut values, "description", "provider operation")?;
    if identity.trim().is_empty() || description.trim().is_empty() {
        return Err(WombatError::configuration(
            "provider operation identity and description must not be empty",
        ));
    }
    let elevated = match values.remove("elevated") {
        None => false,
        Some(FrozenValue::Boolean(value)) => value,
        Some(_) => {
            return Err(WombatError::configuration(
                "provider operation `elevated` must be boolean",
            ));
        }
    };
    let data = values.remove("data").unwrap_or_else(FrozenValue::empty_map);
    if !matches!(data, FrozenValue::Map(_)) {
        return Err(WombatError::configuration(
            "provider operation data must be a string-keyed map",
        ));
    }
    reject_unknown_options(&values, "provider operation")?;
    Ok(ProviderPreparation {
        provider: provider.to_string(),
        identity,
        description,
        elevated,
        data,
    })
}

fn frozen_binding(binding: &ProviderBinding) -> Result<FrozenValue> {
    Ok(serde_json::from_value(serde_json::to_value(binding)?)?)
}

fn install_provider_require(
    lua: &Lua,
    state: Rc<RefCell<RuntimeState>>,
    provider_name: &str,
    api: Table,
) -> Result<()> {
    let provider_name = provider_name.to_string();
    let cache = Rc::new(RefCell::new(BTreeMap::<String, mlua::RegistryKey>::new()));
    let require = lua.create_function(move |lua, module: String| {
        if module == "wombat.provider" {
            return Ok(Value::Table(api.clone()));
        }
        validate_provider_module_name(&module).map_err(mlua::Error::external)?;
        if let Some(key) = cache.borrow().get(&module) {
            return lua.registry_value(key);
        }
        let relative = module.replace('.', "/");
        let root = state.borrow().root.join("providers").join(&provider_name);
        let direct = root.join(format!("{relative}.lua"));
        let initial = root.join(&relative).join("init.lua");
        let path = if direct.is_file() {
            direct
        } else if initial.is_file() {
            initial
        } else {
            return Err(mlua::Error::external(WombatError::configuration(format!(
                "provider `{provider_name}` cannot find helper module `{module}`"
            ))));
        };
        let source = load_tracked_source(&state, &path).map_err(mlua::Error::external)?;
        let name = display_path(&state.borrow().root, &path);
        let value: Value = lua.load(&source).set_name(format!("@{name}")).eval()?;
        let value = if value.is_nil() {
            Value::Boolean(true)
        } else {
            value
        };
        let key = lua.create_registry_value(value.clone())?;
        cache.borrow_mut().insert(module, key);
        Ok(value)
    })?;
    lua.globals().set("require", require)?;
    Ok(())
}

fn validate_provider_module_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.split('.').any(|part| {
            part.is_empty()
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
    {
        return Err(WombatError::configuration(format!(
            "invalid provider helper module `{name}`"
        )));
    }
    Ok(())
}

fn frozen_candidate(candidate: &RequirementCandidate) -> Result<FrozenValue> {
    Ok(serde_json::from_value(serde_json::to_value(candidate)?)?)
}

fn record_provider_sources(state: &Rc<RefCell<RuntimeState>>, provider_name: &str) -> Result<()> {
    let root_prefix = format!("providers/{provider_name}/");
    let entrypoint = format!("providers/{provider_name}.lua");
    let mut files = state
        .borrow()
        .sources
        .values()
        .filter(|source| {
            source.manifest.path == entrypoint || source.manifest.path.starts_with(&root_prefix)
        })
        .map(|source| {
            Ok(crate::manifest::ProviderFile {
                source: source.manifest.path.clone(),
                payload: source
                    .manifest
                    .path
                    .strip_prefix("providers/")
                    .expect("provider sources live under providers")
                    .to_string(),
                digest: source.manifest.digest.clone(),
                size: u64::try_from(source.snapshot.len())
                    .map_err(|_| WombatError::configuration("provider source is too large"))?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    files.sort_by(|left, right| left.payload.cmp(&right.payload));
    let mut state = state.borrow_mut();
    let mut found = false;
    if let Some(configured) = state
        .providers
        .iter_mut()
        .find(|provider| provider.name == provider_name)
    {
        found = true;
        if let ProviderOrigin::Custom {
            files: configured_files,
            ..
        } = &mut configured.origin
        {
            *configured_files = files.clone();
        }
    }
    if let Some(configured) = state
        .build_providers
        .iter_mut()
        .find(|provider| provider.name == provider_name)
    {
        found = true;
        if let ProviderOrigin::Custom {
            files: configured_files,
            ..
        } = &mut configured.origin
        {
            *configured_files = files;
        }
    }
    debug_assert!(found, "resolved provider is configured");
    Ok(())
}

fn parse_provider_resolution(
    provider: &str,
    value: FrozenValue,
) -> Result<std::result::Result<ProviderBinding, String>> {
    let FrozenValue::Map(mut values) = value else {
        return Err(WombatError::configuration(format!(
            "provider `{provider}` resolve() must return provider.binding() or provider.unsupported()"
        )));
    };
    let kind = take_string(&mut values, "kind", "provider resolution")?;
    if kind == "unsupported" {
        let reason = take_string(&mut values, "reason", "unsupported provider resolution")?;
        reject_unknown_options(&values, "unsupported provider resolution")?;
        return Ok(Err(reason));
    }
    if kind != "binding" {
        return Err(WombatError::configuration(format!(
            "provider `{provider}` returned unknown resolution kind `{kind}`"
        )));
    }
    let identity = take_string(&mut values, "identity", "provider binding")?;
    let package = take_optional_string(&mut values, "package", "provider binding")?;
    let publications = values
        .remove("publications")
        .map(parse_publications)
        .transpose()?
        .unwrap_or(Publications {
            commands: Vec::new(),
        });
    let data = values.remove("data").unwrap_or_else(FrozenValue::empty_map);
    if !matches!(data, FrozenValue::Map(_)) {
        return Err(WombatError::configuration(
            "provider binding data must be a string-keyed map",
        ));
    }
    reject_unknown_options(&values, "provider binding")?;
    Ok(Ok(ProviderBinding {
        provider: provider.to_string(),
        identity,
        package,
        publications,
        data,
    }))
}

fn provider_lua_error(provider: &str, phase: &str, error: mlua::Error) -> WombatError {
    WombatError::configuration(format!("provider `{provider}` {phase} failed: {error}"))
}

fn resolved_requirement_handle(requirement: &Requirement) -> FrozenValue {
    let selected = &requirement.candidates[requirement.selected as usize];
    let mut values = BTreeMap::from([
        (
            "kind".to_string(),
            FrozenValue::String(
                match requirement.kind {
                    RequirementKind::Command => "command",
                    RequirementKind::Package => "package",
                }
                .to_string(),
            ),
        ),
        (
            "name".to_string(),
            FrozenValue::String(selected.name().to_string()),
        ),
        (
            "choice".to_string(),
            FrozenValue::String(
                match requirement.choice {
                    RequirementChoice::Required => "required",
                    RequirementChoice::Preferred => "preferred",
                    RequirementChoice::Accepted => "accepted",
                }
                .to_string(),
            ),
        ),
        (
            "alternative".to_string(),
            FrozenValue::Integer(i64::from(requirement.selected) + 1),
        ),
        (
            "provider".to_string(),
            FrozenValue::String(requirement.binding.provider.clone()),
        ),
        (
            "publications".to_string(),
            FrozenValue::Map(BTreeMap::from([(
                "commands".to_string(),
                FrozenValue::Array(
                    requirement
                        .binding
                        .publications
                        .commands
                        .iter()
                        .cloned()
                        .map(FrozenValue::String)
                        .collect(),
                ),
            )])),
        ),
    ]);
    if let Some(minimum) = selected.minimum() {
        values.insert(
            "minimum".to_string(),
            FrozenValue::String(minimum.to_string()),
        );
    }
    if let Some(package) = &requirement.binding.package {
        values.insert("package".to_string(), FrozenValue::String(package.clone()));
    }
    FrozenValue::Map(values)
}

fn take_string(
    options: &mut BTreeMap<String, FrozenValue>,
    key: &str,
    subject: &str,
) -> Result<String> {
    match options.remove(key) {
        Some(FrozenValue::String(value)) if !value.is_empty() => Ok(value),
        _ => Err(WombatError::configuration(format!(
            "{subject} requires a non-empty string `{key}`"
        ))),
    }
}

fn take_optional_string(
    options: &mut BTreeMap<String, FrozenValue>,
    key: &str,
    subject: &str,
) -> Result<Option<String>> {
    match options.remove(key) {
        None => Ok(None),
        Some(FrozenValue::String(value)) => Ok(Some(value)),
        Some(_) => Err(WombatError::configuration(format!(
            "{subject} `{key}` must be a string"
        ))),
    }
}

fn reject_unknown_options(options: &BTreeMap<String, FrozenValue>, subject: &str) -> Result<()> {
    if let Some(key) = options.keys().next() {
        return Err(WombatError::configuration(format!(
            "{subject} does not support option `{key}`"
        )));
    }
    Ok(())
}

fn validate_provider_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || (index > 0 && matches!(byte, b'-' | b'_'))
        })
    {
        return Err(WombatError::configuration(format!(
            "invalid provider name `{name}`; expected lowercase letters, digits, `-`, or `_`"
        )));
    }
    Ok(())
}

fn validate_product_name(name: &str, kind: RequirementKind) -> Result<()> {
    let valid = match kind {
        RequirementKind::Command => {
            !name.is_empty()
                && !name.starts_with('-')
                && name.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'+' | b'@')
                })
        }
        RequirementKind::Package => {
            !name.is_empty()
                && !name.starts_with('-')
                && !name
                    .bytes()
                    .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        }
    };
    if !valid {
        return Err(WombatError::configuration(format!(
            "invalid {} requirement name `{name}`",
            match kind {
                RequirementKind::Command => "command",
                RequirementKind::Package => "package",
            }
        )));
    }
    Ok(())
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

fn declare_generated(
    state: &Rc<RefCell<RuntimeState>>,
    name: &str,
    options: Value,
    location: Location,
) -> Result<()> {
    validate_relative_path(name, "generated artifact name")?;
    let Value::Table(options) = options else {
        return Err(WombatError::configuration(
            "w.generate() requires an options table",
        ));
    };
    let keys = options
        .clone()
        .pairs::<Value, Value>()
        .map(|pair| pair.map(|(key, _)| key))
        .collect::<mlua::Result<Vec<_>>>()?;
    for key in keys {
        let Value::String(key) = key else {
            return Err(WombatError::configuration(
                "w.generate() option names must be strings",
            ));
        };
        let key = key.to_str()?;
        if !matches!(key.as_ref(), "content" | "to" | "executable") {
            return Err(WombatError::configuration(format!(
                "w.generate() does not support option `{key}`"
            )));
        }
    }
    let content = options.get::<Value>("content")?;
    let Value::String(content) = content else {
        return Err(WombatError::configuration(
            "w.generate() requires binary-safe string `content`",
        ));
    };
    let bytes = content.as_bytes().to_vec();
    let explicit_target = options.get::<Option<String>>("to")?;
    let executable = options.get::<Option<bool>>("executable")?.unwrap_or(false);

    let mut state = state.borrow_mut();
    let (_, module_anchor) = state.active_location();
    let owner = state.active_module().unwrap_or(ROOT_MODULE).to_string();
    let target = match explicit_target.as_deref() {
        Some(target) => parse_explicit_target(target)?,
        None => {
            let anchor = module_anchor.ok_or_else(|| {
                WombatError::configuration(format!(
                    "cannot infer a target for generated artifact `{name}` from root policy; provide `to` at {}",
                    location.display()
                ))
            })?;
            infer_target(anchor, name, InferenceBasis::ModuleAnchor)?
        }
    };
    state.root_policy_started = true;
    state.artifacts.push(EvaluatedArtifact {
        kind: ArtifactKind::File,
        source: format!(
            "generated/{}/{}",
            if owner == ROOT_MODULE { "root" } else { &owner },
            name
        ),
        source_origin: SourceOrigin::Generated {
            name: name.to_string(),
        },
        production: EvaluatedProduction::GeneratedLua {
            content: bytes,
            executable,
        },
        target,
        fingerprint: None,
        owner,
        declared_at: location.trace,
    });
    Ok(())
}

fn declare_task(
    lua: &Lua,
    state: &Rc<RefCell<RuntimeState>>,
    entrypoint: &str,
    params: Value,
    options: Value,
    location: Location,
) -> Result<()> {
    validate_relative_path(entrypoint, "task entrypoint")?;
    let params = FrozenValue::from_lua(params)?;
    if !matches!(params, FrozenValue::Map(_)) {
        return Err(WombatError::configuration(
            "w.build.task() params must be a string-keyed table",
        ));
    }
    let params_json = serde_json::to_vec(&params)?;
    if params_json.len() > 64 * 1024 {
        return Err(WombatError::configuration(
            "w.build.task() params exceed the 64 KiB argv limit; pass large or binary inputs through files",
        ));
    }
    let frozen = FrozenValue::from_lua(options)?;
    let FrozenValue::Map(mut options) = frozen else {
        return Err(WombatError::configuration(
            "w.build.task() options must be a table",
        ));
    };
    let instance = take_optional_string(&mut options, "name", "task")?;
    if let Some(instance) = &instance
        && (instance.is_empty()
            || !instance
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
    {
        return Err(WombatError::configuration(format!(
            "task instance name `{instance}` is invalid; expected ASCII letters, numbers, `-`, or `_`"
        )));
    }
    let explicit_target = take_optional_string(&mut options, "to", "task")?;
    let python_helper = match options.remove("python_helper") {
        None => true,
        Some(FrozenValue::Boolean(value)) => value,
        Some(_) => {
            return Err(WombatError::configuration(
                "task `python_helper` must be boolean",
            ));
        }
    };
    let logs = match options.remove("logs") {
        None => TaskLogPolicy::Failure,
        Some(FrozenValue::String(value)) if value == "failure" => TaskLogPolicy::Failure,
        Some(FrozenValue::String(value)) if value == "always" => TaskLogPolicy::Always,
        Some(FrozenValue::String(value)) if value == "never" => TaskLogPolicy::Never,
        Some(_) => {
            return Err(WombatError::configuration(
                "task `logs` must be `failure`, `always`, or `never`",
            ));
        }
    };
    let cache = match options.remove("cache") {
        None | Some(FrozenValue::Boolean(true)) => TaskCachePolicy {
            enabled: true,
            revision: None,
        },
        Some(FrozenValue::Boolean(false)) => TaskCachePolicy {
            enabled: false,
            revision: None,
        },
        Some(FrozenValue::Map(mut cache)) => {
            let revision = take_optional_string(&mut cache, "revision", "task cache")?;
            reject_unknown_options(&cache, "task cache")?;
            TaskCachePolicy {
                enabled: true,
                revision,
            }
        }
        Some(_) => {
            return Err(WombatError::configuration(
                "task `cache` must be boolean or a table containing optional `revision`",
            ));
        }
    };

    let root = state.borrow().root.clone();
    let absolute = root.join("tasks").join(entrypoint);
    validate_source_components(&root, &absolute)?;
    let fingerprint = fingerprint_regular_file(&absolute).map_err(|error| {
        error.with_note(format!(
            "task `{entrypoint}` must be a regular file beneath `tasks/`"
        ))
    })?;
    let bytes = fs::read(&absolute).map_err(|error| WombatError::io(&absolute, error))?;
    let entrypoint_digest = digest_bytes(&bytes);
    let runner = parse_task_runner(
        entrypoint,
        options.remove("interpreter"),
        &absolute,
        &state.borrow().task_interpreters,
    )?;
    reject_unknown_options(&options, "task")?;

    if let Some(command) = runner.command.as_deref()
        && runner.family != TaskRunnerFamily::Direct
        && !command.contains('/')
        && !command.contains('\\')
    {
        let _ = declare_requirement(
            lua,
            state,
            RequirementDeclaration {
                kind: "command",
                name: command,
                options: Value::Nil,
                preferred: false,
                location: location.clone(),
                build_scope: true,
            },
        )?;
    }

    let mut state = state.borrow_mut();
    let (_, module_anchor) = state.active_location();
    let owner = state.active_module().unwrap_or(ROOT_MODULE).to_string();
    let identity = format!(
        "{}:{}{}",
        owner,
        entrypoint,
        instance
            .as_ref()
            .map(|value| format!("#{value}"))
            .unwrap_or_default()
    );
    if state
        .tasks
        .iter()
        .any(|task| task.task.identity == identity)
    {
        return Err(WombatError::configuration(format!(
            "task `{identity}` is declared more than once; add a unique `name` option"
        )));
    }
    let target_root = match explicit_target.as_deref() {
        Some(target) => Some(parse_explicit_target_root(target)?),
        None => module_anchor
            .map(|anchor| infer_target_root(anchor, "", InferenceBasis::ModuleAnchor))
            .transpose()?,
    }
    .map(|root| TaskTargetRoot {
        anchor: root.anchor,
        path: root.path,
        origin: root.origin,
    });
    state.root_policy_started = true;
    state.tasks.push(EvaluatedTask {
        task: Task {
            identity,
            owner,
            entrypoint: format!("tasks/{entrypoint}"),
            entrypoint_digest,
            params,
            runner,
            python_helper,
            logs,
            cache,
            target_root,
            declared_at: location.trace,
            outputs: Vec::new(),
        },
        fingerprint,
    });
    Ok(())
}

fn parse_task_runner(
    entrypoint: &str,
    configured: Option<FrozenValue>,
    absolute: &Path,
    configured_defaults: &BTreeMap<String, TaskRunner>,
) -> Result<TaskRunner> {
    if let Some(configured) = configured {
        let (command, args) = match configured {
            FrozenValue::String(command) if !command.is_empty() => (command, Vec::new()),
            FrozenValue::Map(mut values) => {
                let command = take_string(&mut values, "command", "task interpreter")?;
                let args = match values.remove("args") {
                    None => Vec::new(),
                    Some(FrozenValue::Array(values)) => values
                        .into_iter()
                        .map(|value| match value {
                            FrozenValue::String(value) => Ok(value),
                            _ => Err(WombatError::configuration(
                                "task interpreter args must be strings",
                            )),
                        })
                        .collect::<Result<Vec<_>>>()?,
                    Some(_) => {
                        return Err(WombatError::configuration(
                            "task interpreter `args` must be an array",
                        ));
                    }
                };
                reject_unknown_options(&values, "task interpreter")?;
                (command, args)
            }
            _ => {
                return Err(WombatError::configuration(
                    "task `interpreter` must be a non-empty command string or table",
                ));
            }
        };
        validate_interpreter_command(&command)?;
        return Ok(TaskRunner {
            contract_version: 1,
            family: if entrypoint.ends_with(".py") {
                TaskRunnerFamily::Python
            } else {
                TaskRunnerFamily::Custom
            },
            command: Some(command),
            args,
        });
    }
    let configured_name = match Path::new(entrypoint)
        .extension()
        .and_then(|extension| extension.to_str())
    {
        Some("py") => Some("python"),
        Some("sh") => Some("shell"),
        Some("bash") => Some("bash"),
        Some("lua") => Some("lua"),
        _ => None,
    };
    if let Some(runner) = configured_name.and_then(|name| configured_defaults.get(name)) {
        return Ok(runner.clone());
    }
    let (family, command) = match Path::new(entrypoint)
        .extension()
        .and_then(|extension| extension.to_str())
    {
        Some("py") => (TaskRunnerFamily::Python, Some("python3".to_string())),
        Some("sh") => (TaskRunnerFamily::PosixShell, Some("sh".to_string())),
        Some("bash") => (TaskRunnerFamily::Bash, Some("bash".to_string())),
        Some("lua") => (TaskRunnerFamily::EmbeddedLua, None),
        None if source_executable(absolute)? => (TaskRunnerFamily::Direct, None),
        None => {
            return Err(WombatError::configuration(format!(
                "extensionless task `{entrypoint}` must be executable or declare `interpreter`"
            )));
        }
        Some(extension) => {
            return Err(WombatError::configuration(format!(
                "cannot infer an interpreter for task `{entrypoint}` with extension `.{extension}`; declare `interpreter`"
            )));
        }
    };
    Ok(TaskRunner {
        contract_version: 1,
        family,
        command,
        args: Vec::new(),
    })
}

fn validate_interpreter_command(command: &str) -> Result<()> {
    if command.is_empty() {
        return Err(WombatError::configuration(
            "task interpreter command must not be empty",
        ));
    }
    let path = Path::new(command);
    if path.components().count() > 1 && !path.is_absolute() {
        return Err(WombatError::configuration(format!(
            "task interpreter `{command}` must be a bare command or absolute path"
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn source_executable(path: &Path) -> Result<bool> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = fs::metadata(path).map_err(|error| WombatError::io(path, error))?;
    Ok(metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn source_executable(_path: &Path) -> Result<bool> {
    Ok(false)
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
            fingerprint: Some(SourceFingerprint::from_metadata(&metadata)),
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
                fingerprint: Some(leaf.fingerprint.clone()),
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

fn build_manifest(
    state: &RuntimeState,
    build_preparations: Vec<ProviderPreparation>,
    preparations: Vec<ProviderPreparation>,
) -> EvaluatedManifest {
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
        plan_id: String::new(),
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
        build_providers: state.build_providers.clone(),
        build_requirements: state.build_requirements.clone(),
        build_preparations,
        providers: state.providers.clone(),
        requirements: state.requirements.clone(),
        preparations,
        tasks: state.tasks.clone(),
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

pub(crate) fn validate_artifact_conflicts(artifacts: &[EvaluatedArtifact]) -> Result<()> {
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
                SourceOrigin::Generated { name } => {
                    format!("`{}` (generated value `{name}`)", artifact.source)
                }
                SourceOrigin::Task { identity, relative } => format!(
                    "`{}` (task `{identity}` output `{relative}`)",
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
