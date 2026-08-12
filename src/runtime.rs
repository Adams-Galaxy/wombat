use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};

use globset::{Glob, GlobSetBuilder};
use mlua::{Function, Lua, LuaOptions, MultiValue, StdLib, Table, Value};
use sha2::{Digest, Sha256};

use crate::context::{HostContext, ResolvedTarget, TargetOrigin, TargetPlatform};
use crate::frozen::FrozenValue;
use crate::inputs::{self, InputSpec};
use crate::ladder::{CoreRung, ExecutionLadder, LadderRung, RungId};
use crate::manifest::{
    ArtifactKind, ArtifactNotice, ArtifactNoticeKind, ArtifactPolicy, ArtifactSelection,
    ArtifactSelectionKind, BuildInput, Dependency, DependencyKind, EvaluatedArtifact,
    EvaluatedDirectory, EvaluatedManifest, EvaluatedProduction, EvaluatedTask,
    MAX_SOURCE_TRACE_FRAMES, ManifestModule, ModuleSourceBase, Observation, ObservationSubject,
    ProcessEnvironmentChange, ProcessInvocation, ProcessObservation, Provider, ProviderBinding,
    ProviderOrigin, ProviderPreparation, Publications, Requirement, RequirementCandidate,
    RequirementChoice, RequirementKind, ResolutionAttempt, ResolutionOutcome, Script,
    ScriptPayload, ScriptSchedule, ScriptScope, SourceFile, SourceLocation, SourceOrigin,
    SourceTrace, Task, TaskCachePolicy, TaskLogPolicy, TaskRunner, TaskRunnerFamily,
    TaskTargetRoot,
};
use crate::path::{
    infer_target, infer_target_root, parse_explicit_target, parse_explicit_target_root,
    reject_legacy_artifact_trees, validate_relative_path,
};
use crate::selection::{
    compile_selector, hidden_components_authorized, in_static_scope, is_excluded, matcher,
    project_physical,
};
use crate::source::{
    SourceFingerprint, fingerprint_regular_file, snapshot_directory_filtered,
    validate_source_components,
};
use crate::{Diagnostic, Result, WombatError};

const WOMBAT_LUA: &str = include_str!("../lua/wombat/init.lua");
const ROOT_MODULE: &str = "<root>";

fn adjust_log_level(
    level: crate::presentation::LogLevel,
    adjustment: i8,
) -> crate::presentation::LogLevel {
    use crate::presentation::LogLevel::*;
    let levels = [Debug, Info, Notice, Warn, Error];
    let index = levels
        .iter()
        .position(|candidate| *candidate == level)
        .expect("known log level") as i16;
    levels[(index - adjustment as i16).clamp(0, 4) as usize]
}

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
    source_base: Option<ModuleSourceBase>,
    declarations_started: bool,
}

impl ModuleRecord {
    fn selected() -> Self {
        Self {
            explicit_config: None,
            state: EvaluationState::Selected,
            export: None,
            location: None,
            source_base: None,
            declarations_started: false,
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
}

#[derive(Debug)]
struct RuntimeState {
    root: PathBuf,
    sources: BTreeMap<String, TrackedSource>,
    modules: BTreeMap<String, ModuleRecord>,
    dependencies: BTreeSet<Dependency>,
    providers: Vec<Provider>,
    requirements: Vec<Requirement>,
    task_interpreters: BTreeMap<String, TaskRunner>,
    tasks: Vec<EvaluatedTask>,
    scripts: Vec<Script>,
    ladder: Option<ExecutionLadder>,
    next_action_order: u64,
    artifacts: Vec<EvaluatedArtifact>,
    directories: Vec<EvaluatedDirectory>,
    artifact_policy: ArtifactPolicy,
    artifact_notices: Vec<ArtifactNotice>,
    artifact_selections: Vec<ArtifactSelection>,
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
    process_observations: Vec<ProcessObservation>,
    project_help: Option<String>,
    failure_frames: Vec<SourceLocation>,
    failure_tail_call: bool,
    log_level: crate::presentation::LogLevel,
}

impl RuntimeState {
    fn active_module(&self) -> Option<&str> {
        self.stack.last().map(String::as_str)
    }

    fn active_location(&self) -> (PathBuf, String, Option<String>, bool) {
        self.active_module().map_or_else(
            || {
                (
                    self.root.join("src"),
                    String::new(),
                    Some(String::new()),
                    false,
                )
            },
            |module| {
                let record = self
                    .modules
                    .get(module)
                    .expect("an active module must have a resolved location");
                match &record.source_base {
                    Some(base) => (
                        self.root.join(&base.physical),
                        base.logical.clone(),
                        base.target.clone(),
                        base.hidden,
                    ),
                    None => (
                        self.root.join("src"),
                        String::new(),
                        Some(String::new()),
                        false,
                    ),
                }
            },
        )
    }
}

#[derive(Clone, Debug)]
pub(crate) struct EvaluationOptions {
    pub project_arguments: Vec<OsString>,
    pub host: HostContext,
    pub task_interpreters: BTreeMap<String, TaskRunner>,
    pub log_level: Option<crate::presentation::LogLevel>,
    pub log_adjustment: i8,
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
            log_level: None,
            log_adjustment: 0,
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
    reject_legacy_artifact_trees(&root)?;
    let (artifact_policy, configured_log_level, project_config) = crate::project::load(&root)?;
    let log_level = options
        .log_level
        .unwrap_or_else(|| adjust_log_level(configured_log_level, options.log_adjustment));
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
        task_interpreters: options.task_interpreters,
        tasks: Vec::new(),
        scripts: Vec::new(),
        ladder: None,
        next_action_order: 0,
        artifacts: Vec::new(),
        directories: Vec::new(),
        artifact_policy,
        artifact_notices: Vec::new(),
        artifact_selections: Vec::new(),
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
        process_observations: Vec::new(),
        project_help: None,
        failure_frames: Vec::new(),
        failure_tail_call: false,
        log_level,
    }));

    if let Some(config) = project_config {
        let path = root.join(&config.path);
        let metadata = fs::metadata(&path).map_err(|error| WombatError::io(&path, error))?;
        let bytes = fs::read(&path).map_err(|error| WombatError::io(&path, error))?;
        state.borrow_mut().sources.insert(
            config.path.clone(),
            TrackedSource {
                manifest: config,
                fingerprint: SourceFingerprint::from_metadata(&metadata),
                snapshot: String::from_utf8(bytes).map_err(|_| {
                    WombatError::configuration("repository `wombat.toml` must contain valid UTF-8")
                })?,
            },
        );
    }

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
    let preparations = plan_provider_preparations(&state)?;

    Ok(EvaluationOutcome::Manifest(Box::new(build_manifest(
        &state.borrow(),
        preparations,
    )?)))
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
            configure_providers(&providers_state, entries, location).map_err(mlua::Error::external)
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

    let script_state = Rc::clone(&state);
    native.set(
        "declare_script",
        lua.create_function(
            move |lua, (entrypoint, params, options): (String, Value, Value)| {
                let location = caller_location(lua, &script_state);
                declare_script(lua, &script_state, &entrypoint, params, options, location)
                    .map_err(mlua::Error::external)
            },
        )?,
    )?;

    let ladder_state = Rc::clone(&state);
    native.set(
        "declare_ladder",
        lua.create_function(move |lua, (name, rungs): (String, Value)| {
            let location = caller_location(lua, &ladder_state);
            declare_ladder(&ladder_state, &name, rungs, location).map_err(mlua::Error::external)
        })?,
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

    let module_from_state = Rc::clone(&state);
    native.set(
        "module_from",
        lua.create_function(move |lua, (source, target): (Value, Option<String>)| {
            let location = caller_location(lua, &module_from_state);
            declare_module_from(&module_from_state, source, target.as_deref(), location)
                .map_err(mlua::Error::external)
        })?,
    )?;

    native.set(
        "hidden_source",
        lua.create_function(|lua, source: String| {
            let value = lua.create_table()?;
            value.set("__wombat_hidden", source)?;
            Ok(value)
        })?,
    )?;

    let repository_root = state.borrow().root.clone();
    native.set(
        "repository_path",
        repository_root.to_string_lossy().to_string(),
    )?;

    let data_state = Rc::clone(&state);
    native.set(
        "toml_data",
        lua.create_function(move |lua, path: String| {
            let location = caller_location(lua, &data_state);
            read_toml_data(lua, &data_state, &path, location).map_err(mlua::Error::external)
        })?,
    )?;

    let log_state = Rc::clone(&state);
    native.set(
        "log",
        lua.create_function(
            move |lua, (level, message, fields): (String, String, Value)| {
                let location = caller_location(lua, &log_state);
                emit_lua_log(lua, &log_state, &level, &message, fields, location)
                    .map_err(mlua::Error::external)
            },
        )?,
    )?;

    let exec_state = Rc::clone(&state);
    native.set(
        "exec",
        lua.create_function(move |lua, (argv, options): (Value, Value)| {
            let location = caller_location(lua, &exec_state);
            run_observed_process(lua, &exec_state, argv, options, None, location)
                .map_err(mlua::Error::external)
        })?,
    )?;
    let shell_state = Rc::clone(&state);
    native.set(
        "shell",
        lua.create_function(move |lua, (command, options): (String, Value)| {
            let location = caller_location(lua, &shell_state);
            run_observed_process(
                lua,
                &shell_state,
                Value::String(lua.create_string(command)?),
                options,
                Some(()),
                location,
            )
            .map_err(mlua::Error::external)
        })?,
    )?;

    native.set(
        "install_path",
        lua.create_function(
            move |lua,
                  (source, target, kind, context, exclusions, allow_empty): (
                Value,
                Option<String>,
                String,
                Value,
                Vec<String>,
                bool,
            )| {
                let location = caller_location(lua, &state);
                let (source_path, hidden) = decode_source_selector(source)?;
                register_artifact(
                    &state,
                    ArtifactDeclaration {
                        source_path: &source_path,
                        hidden,
                        explicit_target: target.as_deref(),
                        requested_kind: &kind,
                        context,
                        exclusions,
                        allow_empty,
                        location,
                    },
                )
                .map_err(mlua::Error::external)
            },
        )?,
    )?;

    Ok(native)
}

fn read_toml_data(
    lua: &Lua,
    state: &Rc<RefCell<RuntimeState>>,
    declared: &str,
    location: Location,
) -> Result<Value> {
    if declared.is_empty()
        || Path::new(declared).is_absolute()
        || declared
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(WombatError::configuration(format!(
            "w.data.toml() requires a safe repository-relative path at {}",
            location.display()
        )));
    }
    let path = state.borrow().root.join(declared);
    validate_source_components(&state.borrow().root, &path)?;
    let source = load_tracked_source(state, &path)?;
    let value: toml::Value = toml::from_str(&source).map_err(|error| {
        WombatError::configuration(format!(
            "failed to parse TOML data `{declared}` at {}: {error}",
            location.display()
        ))
    })?;
    toml_to_lua(lua, value)
}

fn toml_to_lua(lua: &Lua, value: toml::Value) -> Result<Value> {
    match value {
        toml::Value::String(value) => Ok(Value::String(lua.create_string(value)?)),
        toml::Value::Datetime(value) => Ok(Value::String(lua.create_string(value.to_string())?)),
        toml::Value::Integer(value) => Ok(Value::Integer(value)),
        toml::Value::Float(value) => Ok(Value::Number(value)),
        toml::Value::Boolean(value) => Ok(Value::Boolean(value)),
        toml::Value::Array(values) => {
            let table = lua.create_table_with_capacity(values.len(), 0)?;
            for (index, value) in values.into_iter().enumerate() {
                table.set(index + 1, toml_to_lua(lua, value)?)?;
            }
            Ok(Value::Table(table))
        }
        toml::Value::Table(values) => {
            let table = lua.create_table_with_capacity(0, values.len())?;
            for (key, value) in values {
                table.set(key, toml_to_lua(lua, value)?)?;
            }
            Ok(Value::Table(table))
        }
    }
}

fn emit_lua_log(
    _lua: &Lua,
    state: &Rc<RefCell<RuntimeState>>,
    level: &str,
    message: &str,
    fields: Value,
    location: Location,
) -> Result<()> {
    let Some(level) = crate::presentation::LogLevel::parse(level) else {
        return Err(WombatError::configuration(format!(
            "unknown log level `{level}`"
        )));
    };
    if level < state.borrow().log_level {
        return Ok(());
    }
    let fields = if matches!(fields, Value::Nil) {
        String::new()
    } else {
        format!(
            " {}",
            serde_json::to_string(&FrozenValue::from_lua(fields)?)?
        )
    };
    eprintln!("{:?}: {message}{fields} ({})", level, location.display());
    Ok(())
}

const DEFAULT_MAX_PROCESS_OUTPUT: u64 = 1024 * 1024;
const MAX_PROCESS_OUTPUT: u64 = 64 * 1024 * 1024;

struct ProcessOptions {
    cwd: PathBuf,
    cwd_display: String,
    environment: Vec<ProcessEnvironmentChange>,
    stdin: Option<Vec<u8>>,
    timeout_ms: Option<u64>,
    max_output: u64,
    sensitive: bool,
    shell: Option<String>,
}

fn run_observed_process(
    lua: &Lua,
    state: &Rc<RefCell<RuntimeState>>,
    request: Value,
    options: Value,
    shell_request: Option<()>,
    location: Location,
) -> Result<Value> {
    let root = state.borrow().root.clone();
    let options = parse_process_options(&root, options, location.clone())?;
    let (mut command, invocation) = if shell_request.is_some() {
        let Value::String(command) = request else {
            unreachable!("shell request is a string")
        };
        let command = command.to_str()?.to_string();
        reject_nul(&command, "shell command")?;
        let shell = options
            .shell
            .clone()
            .unwrap_or_else(|| "/bin/sh".to_string());
        let mut process = Command::new(&shell);
        process.arg("-c").arg(&command);
        (process, ProcessInvocation::Shell { command, shell })
    } else {
        let argv = lua_string_array(request, "w.exec() argv")?;
        if argv.is_empty() {
            return Err(WombatError::configuration(
                "w.exec() argv must not be empty",
            ));
        }
        let mut process = Command::new(&argv[0]);
        process.args(&argv[1..]);
        (process, ProcessInvocation::Exec { argv })
    };
    command
        .current_dir(&options.cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for change in &options.environment {
        match &change.value {
            Some(value) => {
                command.env(&change.name, value);
            }
            None => {
                command.env_remove(&change.name);
            }
        }
    }
    if options.stdin.is_some() {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }
    let mut child = command.spawn().map_err(|error| {
        WombatError::configuration(format!(
            "failed to spawn construction process at {}: {error}",
            location.display()
        ))
    })?;
    if let Some(stdin) = options.stdin.as_ref() {
        use std::io::Write as _;
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(stdin)
            .map_err(|error| {
                WombatError::configuration(format!(
                    "failed to write construction process stdin: {error}"
                ))
            })?;
    }
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let stdout_reader = thread::spawn(move || read_process_stream(stdout));
    let stderr_reader = thread::spawn(move || read_process_stream(stderr));
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|error| {
            WombatError::configuration(format!(
                "failed to wait for construction process at {}: {error}",
                location.display()
            ))
        })? {
            break status;
        }
        if let Some(timeout_ms) = options.timeout_ms
            && started.elapsed() >= Duration::from_millis(timeout_ms)
        {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(WombatError::configuration(format!(
                "construction process timed out after {} ms at {}",
                timeout_ms,
                location.display()
            )));
        }
        thread::sleep(Duration::from_millis(5));
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| WombatError::configuration("construction stdout reader panicked"))?
        .map_err(|error| {
            WombatError::configuration(format!("failed to read construction stdout: {error}"))
        })?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| WombatError::configuration("construction stderr reader panicked"))?
        .map_err(|error| {
            WombatError::configuration(format!("failed to read construction stderr: {error}"))
        })?;
    let stdout_size = u64::try_from(stdout.len())
        .map_err(|_| WombatError::configuration("process stdout exceeds u64"))?;
    let stderr_size = u64::try_from(stderr.len())
        .map_err(|_| WombatError::configuration("process stderr exceeds u64"))?;
    if stdout_size > options.max_output || stderr_size > options.max_output {
        return Err(WombatError::configuration(format!(
            "construction process output exceeded the {} byte limit at {}",
            options.max_output,
            location.display()
        )));
    }
    let code = status.code();
    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt as _;
        status.signal()
    };
    #[cfg(not(unix))]
    let signal = None;
    let observation = ProcessObservation {
        invocation,
        cwd: options.cwd_display,
        environment: options.environment,
        stdin_digest: options.stdin.as_ref().map(|value| digest_bytes(value)),
        timeout_ms: options.timeout_ms,
        max_output: options.max_output,
        sensitive: options.sensitive,
        ok: status.success(),
        code,
        signal,
        stdout_size,
        stdout_digest: digest_bytes(&stdout),
        stderr_size,
        stderr_digest: digest_bytes(&stderr),
        declared_at: location.trace.clone(),
    };
    state.borrow_mut().process_observations.push(observation);
    process_result(
        lua,
        status.success(),
        code,
        signal,
        &stdout,
        &stderr,
        location,
    )
}

fn read_process_stream(mut stream: impl std::io::Read) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn parse_process_options(root: &Path, value: Value, location: Location) -> Result<ProcessOptions> {
    let table = match value {
        Value::Nil => None,
        Value::Table(table) => Some(table),
        _ => {
            return Err(WombatError::configuration(
                "process options must be a table",
            ));
        }
    };
    let mut seen = std::collections::BTreeSet::new();
    let mut cwd = root.to_path_buf();
    let mut cwd_display = ".".to_string();
    let mut environment = Vec::new();
    let mut stdin = None;
    let mut timeout_ms = None;
    let mut max_output = DEFAULT_MAX_PROCESS_OUTPUT;
    let mut sensitive = false;
    let mut shell = None;
    if let Some(table) = table {
        for pair in table.pairs::<String, Value>() {
            let (key, value) = pair?;
            seen.insert(key.clone());
            match key.as_str() {
                "cwd" => {
                    let Value::String(path) = value else {
                        return Err(WombatError::configuration("process `cwd` must be a string"));
                    };
                    let path = path.to_str()?.to_string();
                    reject_nul(&path, "cwd")?;
                    let candidate = PathBuf::from(&path);
                    if candidate.is_absolute() {
                        cwd = candidate;
                        cwd_display = path;
                    } else {
                        if candidate
                            .components()
                            .any(|part| !matches!(part, Component::Normal(_)))
                        {
                            return Err(WombatError::configuration(format!(
                                "process cwd `{path}` escapes the repository at {}",
                                location.display()
                            )));
                        }
                        cwd = root.join(&candidate);
                        cwd_display = path;
                    }
                }
                "env" => {
                    let Value::Table(values) = value else {
                        return Err(WombatError::configuration(
                            "process `env` must be a string-keyed table",
                        ));
                    };
                    for pair in values.pairs::<String, Value>() {
                        let (name, value) = pair?;
                        if name.is_empty() || name.contains('=') || name.contains('\0') {
                            return Err(WombatError::configuration(
                                "process environment name is invalid",
                            ));
                        }
                        let value = match value {
                            Value::Boolean(false) => None,
                            Value::String(value) => {
                                let value = value.to_str()?.to_string();
                                reject_nul(&value, "environment value")?;
                                Some(value)
                            }
                            _ => {
                                return Err(WombatError::configuration(
                                    "process environment values must be strings or false",
                                ));
                            }
                        };
                        environment.push(ProcessEnvironmentChange { name, value });
                    }
                    environment.sort();
                }
                "stdin" => {
                    let Value::String(input) = value else {
                        return Err(WombatError::configuration(
                            "process `stdin` must be a string",
                        ));
                    };
                    stdin = Some(input.as_bytes().to_vec());
                }
                "timeout" => {
                    let seconds = match value {
                        Value::Integer(value) if value > 0 => value as f64,
                        Value::Number(value) if value.is_finite() && value > 0.0 => value,
                        _ => {
                            return Err(WombatError::configuration(
                                "process `timeout` must be a positive number",
                            ));
                        }
                    };
                    timeout_ms = Some((seconds * 1000.0).round() as u64);
                }
                "max_output" => {
                    let Value::Integer(value) = value else {
                        return Err(WombatError::configuration(
                            "process `max_output` must be an integer",
                        ));
                    };
                    max_output = u64::try_from(value).map_err(|_| {
                        WombatError::configuration("process `max_output` must be positive")
                    })?;
                    if max_output == 0 || max_output > MAX_PROCESS_OUTPUT {
                        return Err(WombatError::configuration(
                            "process `max_output` must be between 1 and 67108864",
                        ));
                    }
                }
                "sensitive" => {
                    let Value::Boolean(value) = value else {
                        return Err(WombatError::configuration(
                            "process `sensitive` must be boolean",
                        ));
                    };
                    sensitive = value;
                }
                "shell" => {
                    let Value::String(value) = value else {
                        return Err(WombatError::configuration(
                            "process `shell` must be a string",
                        ));
                    };
                    let value = value.to_str()?.to_string();
                    if !Path::new(&value).is_absolute() {
                        return Err(WombatError::configuration(
                            "process `shell` must be an absolute path",
                        ));
                    }
                    shell = Some(value);
                }
                _ => {
                    return Err(WombatError::configuration(format!(
                        "unknown process option `{key}`"
                    )));
                }
            }
        }
    }
    if !cwd.is_dir() {
        return Err(WombatError::configuration(format!(
            "process cwd `{}` is not a directory",
            cwd.display()
        )));
    }
    Ok(ProcessOptions {
        cwd,
        cwd_display,
        environment,
        stdin,
        timeout_ms,
        max_output,
        sensitive,
        shell,
    })
}

fn lua_string_array(value: Value, context: &str) -> Result<Vec<String>> {
    let Value::Table(values) = value else {
        return Err(WombatError::configuration(format!(
            "{context} must be an array of strings"
        )));
    };
    let mut output = Vec::new();
    for value in values.sequence_values::<String>() {
        let value = value?;
        reject_nul(&value, context)?;
        output.push(value);
    }
    if values.raw_len() != output.len() {
        return Err(WombatError::configuration(format!(
            "{context} must be a contiguous array of strings"
        )));
    }
    Ok(output)
}

fn reject_nul(value: &str, context: &str) -> Result<()> {
    if value.contains('\0') {
        Err(WombatError::configuration(format!(
            "{context} must not contain NUL bytes"
        )))
    } else {
        Ok(())
    }
}

fn process_result(
    lua: &Lua,
    ok: bool,
    code: Option<i32>,
    signal: Option<i32>,
    stdout: &[u8],
    stderr: &[u8],
    location: Location,
) -> Result<Value> {
    let result = lua.create_table()?;
    result.set("ok", ok)?;
    result.set("code", code)?;
    result.set("signal", signal)?;
    result.set("stdout", lua.create_string(stdout)?)?;
    result.set("stderr", lua.create_string(stderr)?)?;
    let check = lua.create_function(move |_, table: Table| {
        if table.get::<bool>("ok")? {
            return Ok(table);
        }
        let stderr: mlua::LuaString = table.get("stderr")?;
        Err::<Table, _>(mlua::Error::external(WombatError::configuration(format!(
            "construction process failed at {}: {}",
            location.display(),
            String::from_utf8_lossy(&stderr.as_bytes())
        ))))
    })?;
    result.set("check", check)?;
    let meta = lua.create_table()?;
    meta.set(
        "__newindex",
        lua.create_function(|_, (): ()| {
            Err::<(), _>(mlua::Error::external("process results are immutable"))
        })?,
    )?;
    result.set_metatable(Some(meta))?;
    Ok(Value::Table(result))
}

fn decode_source_selector(value: Value) -> mlua::Result<(String, bool)> {
    match value {
        Value::String(value) => Ok((value.to_str()?.to_string(), false)),
        Value::Table(value) => {
            let source = value
                .get::<Option<String>>("__wombat_hidden")?
                .ok_or_else(|| {
                    mlua::Error::external("artifact source must be a string or w.hidden() value")
                })?;
            Ok((source, true))
        }
        _ => Err(mlua::Error::external(
            "artifact source must be a string or w.hidden() value",
        )),
    }
}

fn declare_module_from(
    state: &Rc<RefCell<RuntimeState>>,
    source: Value,
    explicit_target: Option<&str>,
    location: Location,
) -> Result<()> {
    let (declared, hidden) = decode_source_selector(source).map_err(WombatError::from)?;
    let selector = compile_selector(&declared, hidden)?;
    if selector.glob {
        return Err(WombatError::configuration(
            "w.module.from() requires an exact source directory, not a glob",
        ));
    }
    let mut state = state.borrow_mut();
    let name = state
        .active_module()
        .ok_or_else(|| {
            WombatError::configuration("w.module.from() may only be called from a selected module")
        })?
        .to_string();
    let record = state.modules.get(&name).expect("active module exists");
    if record.declarations_started {
        return Err(WombatError::configuration(format!(
            "w.module.from() must run before artifact or task declarations at {}",
            location.display()
        )));
    }
    if record.source_base.is_some() {
        return Err(WombatError::configuration(format!(
            "module `{name}` declares w.module.from() more than once"
        )));
    }
    let physical_relative = if selector.physical == "." {
        String::new()
    } else {
        selector.physical.clone()
    };
    let physical = if physical_relative.is_empty() {
        "src".to_string()
    } else {
        format!("src/{physical_relative}")
    };
    let absolute = state.root.join(&physical);
    let metadata =
        fs::symlink_metadata(&absolute).map_err(|error| WombatError::io(&absolute, error))?;
    if !metadata.file_type().is_dir() {
        return Err(WombatError::configuration(format!(
            "module source base `{physical}` must be a directory"
        )));
    }
    let projection = if physical_relative.is_empty() {
        crate::manifest::SourceProjection {
            physical: String::new(),
            logical: String::new(),
            allocated: true,
            hidden,
            components: Vec::new(),
        }
    } else {
        project_physical(&physical_relative, hidden)?
    };
    let target = match explicit_target {
        Some(target) => Some(parse_explicit_target_root(target)?.path),
        None if projection.allocated => Some(projection.logical.clone()),
        None => None,
    };
    state
        .modules
        .get_mut(&name)
        .expect("active module exists")
        .source_base = Some(ModuleSourceBase {
        declared,
        expanded: selector.expanded,
        physical,
        logical: projection.logical,
        target,
        hidden,
    });
    Ok(())
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
                "w.providers()",
                location.display()
            )));
        }
        let already_declared = !state.providers.is_empty();
        if already_declared {
            return Err(WombatError::configuration(format!(
                "{} may be declared only once; repeated at {}",
                "w.providers()",
                location.display()
            )));
        }
        if !state.modules.is_empty()
            || !state.artifacts.is_empty()
            || !state.requirements.is_empty()
            || !state.tasks.is_empty()
        {
            return Err(WombatError::configuration(format!(
                "{} must run before use(), using(), install(), need(), generate(), or build.task() at {}",
                "w.providers()",
                location.display()
            )));
        }
        state.root_policy_started = true;
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
    state.borrow_mut().providers = configured;
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
    } = declaration;
    let options = if options.is_nil() {
        FrozenValue::empty_map()
    } else {
        FrozenValue::from_lua(options)?
    };
    let FrozenValue::Map(mut options) = options else {
        return Err(WombatError::configuration(format!(
            "w.{}.{}() options must be a table",
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
    let when = match options.remove("when") {
        None => CoreRung::MaterialiseBefore.into(),
        Some(FrozenValue::String(value)) => RungId::new(value)?,
        Some(_) => {
            return Err(WombatError::configuration(
                "requirement `when` must be a w.rungs handle or canonical rung string",
            ));
        }
    };
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

    let (owner, providers, target) = {
        let mut state = state.borrow_mut();
        let providers = state.providers.clone();
        if providers.is_empty() {
            return Err(WombatError::configuration(format!(
                "requirements need provider policy; call w.providers() before {} at {}",
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
            effective_target(&state),
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
        when,
    };
    let handle = resolved_requirement_handle(&requirement);
    state.borrow_mut().requirements.push(requirement);
    readonly_frozen(lua, handle).map_err(WombatError::from)
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
) -> Result<Vec<ProviderPreparation>> {
    let (providers, requirements, target) = {
        let state = state.borrow();
        (
            state.providers.clone(),
            state.requirements.clone(),
            effective_target(&state),
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
    let (_, _, module_target, _) = state.active_location();
    let owner = state.active_module().unwrap_or(ROOT_MODULE).to_string();
    let target = match explicit_target.as_deref() {
        Some(target) => parse_explicit_target(target)?,
        None => {
            let base = module_target.ok_or_else(|| {
                WombatError::configuration(format!(
                    "cannot infer a target for generated artifact `{name}` from an unallocated module; provide `to` at {}",
                    location.display()
                ))
            })?;
            infer_target(
                &crate::path::join_relative(&base, name),
                format!("generated:{name}"),
            )?
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
        source_projection: None,
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
    let at = match options.remove("at") {
        None => CoreRung::MaterialiseTasks.into(),
        Some(FrozenValue::String(value)) => RungId::new(value)?,
        Some(_) => {
            return Err(WombatError::configuration(
                "task `at` must be a w.rungs handle or canonical rung string",
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
        && !state.borrow().providers.is_empty()
    {
        let requirement_options = lua.create_table()?;
        requirement_options.set("when", at.id())?;
        let _ = declare_requirement(
            lua,
            state,
            RequirementDeclaration {
                kind: "command",
                name: command,
                options: Value::Table(requirement_options),
                preferred: false,
                location: location.clone(),
            },
        )?;
    }

    let mut state = state.borrow_mut();
    let (_, _, module_target, _) = state.active_location();
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
        None => module_target
            .map(|base| infer_target_root(&base, format!("module:{owner}")))
            .transpose()?,
    }
    .map(|root| TaskTargetRoot {
        path: root.path,
        origin: root.origin,
    });
    let declaration_order = state.next_action_order;
    state.next_action_order += 1;
    state.root_policy_started = true;
    state.tasks.push(EvaluatedTask {
        task: Task {
            identity,
            declaration_order,
            owner,
            entrypoint: format!("tasks/{entrypoint}"),
            entrypoint_digest,
            params,
            runner,
            python_helper,
            logs,
            cache,
            at,
            target_root,
            declared_at: location.trace,
            outputs: Vec::new(),
        },
        fingerprint,
    });
    Ok(())
}

fn declare_ladder(
    state: &Rc<RefCell<RuntimeState>>,
    name: &str,
    rungs: Value,
    location: Location,
) -> Result<()> {
    let frozen = FrozenValue::from_lua(rungs)?;
    let roots = parse_ladder_rungs(frozen, "ladder")?;
    let ladder = ExecutionLadder::new(name.to_string(), roots)
        .map_err(|error| error.with_note(format!("declared at {}", location.display())))?;
    let mut state = state.borrow_mut();
    if state.active_module().is_some() {
        return Err(WombatError::configuration(format!(
            "w.ladder() may only be called from root configuration at {}",
            location.display()
        )));
    }
    if state.ladder.is_some() {
        return Err(WombatError::configuration(format!(
            "root configuration selects more than one ladder at {}",
            location.display()
        )));
    }
    state.root_policy_started = true;
    state.ladder = Some(ladder);
    Ok(())
}

fn parse_ladder_rungs(value: FrozenValue, subject: &str) -> Result<Vec<LadderRung>> {
    let values = match value {
        FrozenValue::Array(values) => values,
        FrozenValue::Map(values) if values.is_empty() => Vec::new(),
        _ => {
            return Err(WombatError::configuration(format!(
                "{subject} rungs must be an array"
            )));
        }
    };
    values
        .into_iter()
        .map(|value| {
            let FrozenValue::Map(mut node) = value else {
                return Err(WombatError::configuration(format!(
                    "{subject} entries must be rung values"
                )));
            };
            let id = RungId::new(take_string(&mut node, "id", "ladder rung")?)?;
            let children = parse_ladder_rungs(
                node.remove("children")
                    .unwrap_or_else(|| FrozenValue::Array(Vec::new())),
                "nested ladder",
            )?;
            reject_unknown_options(&node, "ladder rung")?;
            Ok(LadderRung { id, children })
        })
        .collect()
}

fn declare_script(
    lua: &Lua,
    state: &Rc<RefCell<RuntimeState>>,
    entrypoint: &str,
    params: Value,
    options: Value,
    location: Location,
) -> Result<()> {
    validate_relative_path(entrypoint, "script entrypoint")?;
    let params = FrozenValue::from_lua(params)?;
    if !matches!(params, FrozenValue::Map(_)) {
        return Err(WombatError::configuration(
            "w.script() params must be a string-keyed table",
        ));
    }
    if serde_json::to_vec(&params)?.len() > 64 * 1024 {
        return Err(WombatError::configuration(
            "w.script() params exceed the 64 KiB argv limit; pass large or binary inputs through files",
        ));
    }
    let FrozenValue::Map(mut options) = FrozenValue::from_lua(options)? else {
        return Err(WombatError::configuration(
            "w.script() options must be a table",
        ));
    };
    let name = take_optional_string(&mut options, "name", "script")?;
    if let Some(name) = &name
        && (name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
    {
        return Err(WombatError::configuration(format!(
            "script name `{name}` is invalid; expected ASCII letters, numbers, `-`, or `_`"
        )));
    }
    let at = match options.remove("at") {
        None => CoreRung::MaterialiseBefore.into(),
        Some(FrozenValue::String(value)) => RungId::new(value)?,
        Some(_) => {
            return Err(WombatError::configuration(
                "script `at` must be a rung handle or canonical rung string",
            ));
        }
    };
    let schedule = match options.remove("schedule") {
        None => ScriptSchedule::Always,
        Some(FrozenValue::String(value)) if value == "always" => ScriptSchedule::Always,
        Some(FrozenValue::String(value)) if value == "once" => ScriptSchedule::Once,
        Some(FrozenValue::String(value)) if value == "onchange" => ScriptSchedule::Onchange,
        Some(_) => {
            return Err(WombatError::configuration(
                "script `schedule` must be `always`, `once`, or `onchange`",
            ));
        }
    };
    let scope = match options.remove("scope") {
        None => ScriptScope::Target,
        Some(FrozenValue::String(value)) if value == "target" => ScriptScope::Target,
        Some(FrozenValue::String(value)) if value == "host" => ScriptScope::Host,
        Some(_) => {
            return Err(WombatError::configuration(
                "script `scope` must be `target` or `host`",
            ));
        }
    };
    let python_helper = match options.remove("python_helper") {
        None | Some(FrozenValue::Boolean(true)) => true,
        Some(FrozenValue::Boolean(false)) => false,
        Some(_) => {
            return Err(WombatError::configuration(
                "script `python_helper` must be boolean",
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
                "script `logs` must be `failure`, `always`, or `never`",
            ));
        }
    };
    let files = match options.remove("files") {
        None => Vec::new(),
        Some(FrozenValue::Array(values)) => values
            .into_iter()
            .map(|value| match value {
                FrozenValue::String(value) => Ok(value),
                _ => Err(WombatError::configuration(
                    "script `files` entries must be strings",
                )),
            })
            .collect::<Result<Vec<_>>>()?,
        Some(_) => {
            return Err(WombatError::configuration(
                "script `files` must be an array",
            ));
        }
    };
    let revision = take_optional_string(&mut options, "revision", "script")?;
    let env = match options.remove("env") {
        None => BTreeMap::new(),
        Some(FrozenValue::Map(values)) => values
            .into_iter()
            .map(|(key, value)| match value {
                FrozenValue::String(value) if !key.contains('=') && !key.contains('\0') => {
                    Ok((key, value))
                }
                _ => Err(WombatError::configuration(
                    "script `env` must map valid names to strings",
                )),
            })
            .collect::<Result<BTreeMap<_, _>>>()?,
        Some(_) => {
            return Err(WombatError::configuration(
                "script `env` must be a string map",
            ));
        }
    };
    let timeout_seconds = match options.remove("timeout") {
        None => None,
        Some(FrozenValue::Integer(value)) if value > 0 => Some(
            u64::try_from(value)
                .map_err(|_| WombatError::configuration("script timeout is too large"))?,
        ),
        Some(_) => {
            return Err(WombatError::configuration(
                "script `timeout` must be a positive integer number of seconds",
            ));
        }
    };

    let root = state.borrow().root.clone();
    let scripts_root = root.join("scripts");
    let absolute = scripts_root.join(entrypoint);
    validate_source_components(&root, &absolute)?;
    fingerprint_regular_file(&absolute).map_err(|error| {
        error.with_note(format!(
            "script `{entrypoint}` must be a regular file beneath `scripts/`"
        ))
    })?;
    let runner = parse_task_runner(
        entrypoint,
        options.remove("interpreter"),
        &absolute,
        &state.borrow().task_interpreters,
    )?;
    reject_unknown_options(&options, "script")?;
    let payloads = collect_script_payloads(&root, entrypoint, &files)?;

    if let Some(command) = runner.command.as_deref()
        && runner.family != TaskRunnerFamily::Direct
        && !command.contains('/')
        && !command.contains('\\')
        && !state.borrow().providers.is_empty()
    {
        let requirement_options = lua.create_table()?;
        requirement_options.set("when", at.id())?;
        let _ = declare_requirement(
            lua,
            state,
            RequirementDeclaration {
                kind: "command",
                name: command,
                options: Value::Table(requirement_options),
                preferred: false,
                location: location.clone(),
            },
        )?;
    }

    let mut state = state.borrow_mut();
    let owner = state.active_module().unwrap_or(ROOT_MODULE).to_string();
    let identity = format!(
        "{}:{}{}:{}:{}",
        owner,
        entrypoint,
        name.as_ref()
            .map(|value| format!("#{value}"))
            .unwrap_or_default(),
        match scope {
            ScriptScope::Target => "target",
            ScriptScope::Host => "host",
        },
        at.id(),
    );
    if state
        .scripts
        .iter()
        .any(|script| script.identity == identity)
    {
        return Err(WombatError::configuration(format!(
            "script `{identity}` is declared more than once; add a unique `name` option"
        )));
    }
    let declaration_order = state.next_action_order;
    state.next_action_order += 1;
    state.root_policy_started = true;
    state.scripts.push(Script {
        identity,
        declaration_order,
        owner,
        entrypoint: format!("scripts/{entrypoint}"),
        params,
        runner,
        python_helper,
        logs,
        at,
        schedule,
        scope,
        payloads,
        revision,
        env,
        timeout_seconds,
        declared_at: location.trace,
    });
    Ok(())
}

fn collect_script_payloads(
    root: &Path,
    entrypoint: &str,
    patterns: &[String],
) -> Result<Vec<ScriptPayload>> {
    const MAX_SCRIPT_FILES: usize = 4_096;
    const MAX_SCRIPT_FILE_SIZE: u64 = 16 * 1024 * 1024;
    let scripts_root = root.join("scripts");
    let mut selected = BTreeSet::from([entrypoint.to_string()]);
    if !patterns.is_empty() {
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            validate_relative_path(pattern, "script companion pattern")?;
            builder.add(Glob::new(pattern).map_err(|error| {
                WombatError::configuration(format!(
                    "invalid script companion glob `{pattern}`: {error}"
                ))
            })?);
        }
        let matcher = builder.build().map_err(|error| {
            WombatError::configuration(format!("invalid script companion globs: {error}"))
        })?;
        let mut pending = vec![scripts_root.clone()];
        while let Some(directory) = pending.pop() {
            let mut entries = fs::read_dir(&directory)
                .map_err(|error| WombatError::io(&directory, error))?
                .collect::<std::io::Result<Vec<_>>>()
                .map_err(|error| WombatError::io(&directory, error))?;
            entries.sort_by_key(std::fs::DirEntry::file_name);
            for entry in entries {
                let path = entry.path();
                let metadata =
                    fs::symlink_metadata(&path).map_err(|error| WombatError::io(&path, error))?;
                if metadata.file_type().is_symlink() {
                    return Err(WombatError::configuration(format!(
                        "script payload `{}` must not be a symbolic link",
                        path.display()
                    )));
                }
                if metadata.is_dir() {
                    pending.push(path);
                } else if metadata.is_file() {
                    let relative = path
                        .strip_prefix(&scripts_root)
                        .expect("walk remains in scripts")
                        .to_string_lossy()
                        .replace('\\', "/");
                    if matcher.is_match(&relative) {
                        selected.insert(relative);
                    }
                }
            }
        }
    }
    if selected.len() > MAX_SCRIPT_FILES {
        return Err(WombatError::configuration(
            "script payload exceeds 4096 files",
        ));
    }
    selected
        .into_iter()
        .map(|relative| {
            let path = scripts_root.join(&relative);
            validate_source_components(root, &path)?;
            let metadata = fs::metadata(&path).map_err(|error| WombatError::io(&path, error))?;
            if !metadata.is_file() || metadata.len() > MAX_SCRIPT_FILE_SIZE {
                return Err(WombatError::configuration(format!(
                    "script payload `{relative}` must be a regular file no larger than 16 MiB"
                )));
            }
            let bytes = fs::read(&path).map_err(|error| WombatError::io(&path, error))?;
            Ok(ScriptPayload {
                source: format!("scripts/{relative}"),
                relative,
                digest: digest_bytes(&bytes),
                size: metadata.len(),
                executable: source_executable(&path)?,
            })
        })
        .collect()
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

struct ArtifactDeclaration<'a> {
    source_path: &'a str,
    hidden: bool,
    explicit_target: Option<&'a str>,
    requested_kind: &'a str,
    context: Value,
    exclusions: Vec<String>,
    allow_empty: bool,
    location: Location,
}

fn register_artifact(
    state: &Rc<RefCell<RuntimeState>>,
    declaration: ArtifactDeclaration<'_>,
) -> Result<()> {
    let ArtifactDeclaration {
        source_path,
        hidden,
        explicit_target,
        requested_kind,
        context,
        exclusions,
        allow_empty,
        location,
    } = declaration;
    if !matches!(requested_kind, "auto" | "file" | "template") {
        return Err(WombatError::configuration(format!(
            "unsupported artifact production kind `{requested_kind}`"
        )));
    }

    let mut selector = compile_selector(source_path, hidden)?;
    let exclusion_matchers = exclusions
        .iter()
        .map(|value| compile_selector(value, hidden).and_then(|value| matcher(&value.physical)))
        .collect::<Result<Vec<_>>>()?;
    let mut state = state.borrow_mut();
    let repository_root = state.root.clone();
    let (source_base, base_logical, base_target, base_hidden) = state.active_location();
    let owner = state.active_module().unwrap_or(ROOT_MODULE).to_string();
    if let Some(module) = state.active_module().map(str::to_string) {
        state
            .modules
            .get_mut(&module)
            .expect("active module exists")
            .declarations_started = true;
    }
    let mut absolute_selection = if selector.physical == "." {
        source_base.clone()
    } else {
        source_base.join(&selector.physical)
    };
    if !selector.glob && !selector.physical.ends_with(".tmpl") {
        let template_physical = format!("{}.tmpl", selector.physical);
        let template_selection = source_base.join(&template_physical);
        let exact_metadata = fs::symlink_metadata(&absolute_selection);
        let template_metadata = fs::symlink_metadata(&template_selection);
        match (&exact_metadata, &template_metadata) {
            (Ok(exact), Ok(template))
                if exact.file_type().is_file() && template.file_type().is_file() =>
            {
                return Err(WombatError::configuration(format!(
                    "artifact source `{source_path}` is ambiguous: both `{}` and `{}` exist; name the physical `.tmpl` source explicitly or remove one candidate",
                    display_path(&repository_root, &absolute_selection),
                    display_path(&repository_root, &template_selection),
                )));
            }
            (Err(error), Ok(template))
                if error.kind() == std::io::ErrorKind::NotFound
                    && template.file_type().is_file() =>
            {
                selector.physical = template_physical;
                selector.expanded.push_str(".tmpl");
                absolute_selection = template_selection;
            }
            _ => {}
        }
    }
    let mut selected = Vec::new();
    let mut selected_snapshot = None;
    let mut selected_snapshot_root = source_base.clone();
    let directory_selector = !selector.glob && absolute_selection.is_dir();
    let set_selector = selector.glob || directory_selector;
    if directory_selector && requested_kind != "auto" {
        return Err(WombatError::configuration(format!(
            "install.{requested_kind}() cannot select a directory; use install() for directory selection"
        )));
    }
    if selector.glob {
        let selector_matcher = matcher(&selector.physical)?;
        let snapshot = snapshot_directory_filtered(
            &repository_root,
            &source_base,
            |relative, is_directory| {
                in_static_scope(relative, &selector.static_root)
                    && hidden_components_authorized(relative, &selector.physical)
                    && !is_excluded(&exclusion_matchers, relative, is_directory)
            },
        )?;
        for leaf in &snapshot {
            if selector_matcher.is_match(&leaf.relative) {
                selected.push((leaf.relative.clone(), leaf.fingerprint.clone()));
            }
        }
        selected_snapshot = Some(snapshot);
    } else if absolute_selection.is_dir() {
        let snapshot = snapshot_directory_filtered(
            &repository_root,
            &absolute_selection,
            |relative, is_directory| {
                !relative
                    .split('/')
                    .any(crate::selection::is_hidden_component)
                    && !is_excluded(&exclusion_matchers, relative, is_directory)
            },
        )?;
        for leaf in &snapshot {
            let relative = if selector.physical == "." {
                leaf.relative.clone()
            } else {
                format!("{}/{}", selector.physical, leaf.relative)
            };
            selected.push((relative, leaf.fingerprint.clone()));
        }
        selected_snapshot_root = absolute_selection.clone();
        selected_snapshot = Some(snapshot);
    } else {
        if !exclusions.is_empty() || allow_empty {
            return Err(WombatError::configuration(
                "`exclude` and `allow_empty` are only valid for directory or glob selectors",
            ));
        }
        let metadata = fs::symlink_metadata(&absolute_selection).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                WombatError::configuration(format!(
                    "artifact source `{source_path}` does not exist beneath its declaration base"
                ))
            } else {
                WombatError::io(&absolute_selection, error)
            }
        })?;
        if !metadata.file_type().is_file() {
            return Err(WombatError::configuration(format!(
                "artifact source `{source_path}` must be a regular file or directory"
            )));
        }
        selected.push((
            selector.physical.clone(),
            SourceFingerprint::from_metadata(&metadata),
        ));
    }
    if selected.is_empty() && !(set_selector && allow_empty) {
        return Err(WombatError::configuration(format!(
            "artifact selector `{source_path}` matched no files; set `allow_empty = true` if this is intentional"
        )));
    }
    let context = FrozenValue::from_lua(context)?;
    if !matches!(context, FrozenValue::Null | FrozenValue::Map(_)) {
        return Err(WombatError::configuration(
            "template `with` context must be a string-keyed map",
        ));
    }
    let explicit_root = explicit_target
        .map(parse_explicit_target_root)
        .transpose()?;
    let selection_root = if selector.glob {
        selector.static_root.trim_end_matches('/').to_string()
    } else if set_selector {
        selector.physical.clone()
    } else {
        selector
            .physical
            .rsplit_once('/')
            .map_or("", |(root, _)| root)
            .to_string()
    };
    let mut skipped = Vec::new();
    let mut matched = Vec::new();
    for (relative, fingerprint) in selected {
        let hidden_authorized = hidden_components_authorized(&relative, &selector.physical);
        if relative
            .split('/')
            .any(crate::selection::is_hidden_component)
            && !hidden_authorized
        {
            continue;
        }
        let mut projection = project_physical(&relative, hidden_authorized)?;
        let relative_from_root = relative
            .strip_prefix(&selection_root)
            .unwrap_or(&relative)
            .trim_start_matches('/');
        let relative_projection = if relative_from_root.is_empty() {
            projection.clone()
        } else {
            project_physical(
                relative_from_root,
                hidden_components_authorized(relative_from_root, &selector.physical),
            )?
        };
        let projected_relative = relative_projection.logical.clone();
        let target_path = if !set_selector {
            explicit_target.map(str::to_string).or_else(|| {
                projection
                    .allocated
                    .then(|| {
                        base_target
                            .as_ref()
                            .map(|base| crate::path::join_relative(base, &projection.logical))
                    })
                    .flatten()
            })
        } else if let Some(root) = &explicit_root {
            relative_projection
                .allocated
                .then(|| crate::path::join_relative(&root.path, &projected_relative))
        } else if projection.allocated {
            base_target
                .as_ref()
                .map(|base| crate::path::join_relative(base, &projection.logical))
        } else {
            None
        };
        let Some(mut target_path) = target_path else {
            skipped.push(relative.clone());
            continue;
        };
        let template = match requested_kind {
            "template" => true,
            "file" => false,
            _ => {
                relative.ends_with(".tmpl")
                    || (!set_selector && !matches!(context, FrozenValue::Null))
            }
        };
        if template && explicit_target.is_none() {
            target_path = target_path
                .strip_suffix(".tmpl")
                .unwrap_or(&target_path)
                .to_string();
        }
        let source = display_path(&repository_root, &source_base.join(&relative));
        projection.physical = source.clone();
        let origin = if set_selector {
            SourceOrigin::Directory {
                declared: source_path.to_string(),
                expanded: selector.expanded.clone(),
                root: display_path(&repository_root, &source_base.join(&selection_root)),
                relative: relative_from_root.to_string(),
                exclusions: exclusions.clone(),
                allow_empty,
            }
        } else {
            SourceOrigin::Direct {
                declared: source_path.to_string(),
                expanded: selector.expanded.clone(),
            }
        };
        let target = if !set_selector && explicit_target.is_some() {
            parse_explicit_target(&target_path)?
        } else if let Some(root) = &explicit_root {
            crate::manifest::TargetPath {
                path: target_path,
                origin: crate::manifest::TargetOrigin::DirectoryExplicit {
                    declared: root.path.clone(),
                    relative: projected_relative,
                },
            }
        } else {
            infer_target(&target_path, source.clone())?
        };
        state.artifacts.push(EvaluatedArtifact {
            kind: ArtifactKind::File,
            source,
            source_origin: origin,
            source_projection: Some(projection),
            production: if template {
                EvaluatedProduction::Template {
                    context: match &context {
                        FrozenValue::Null => FrozenValue::empty_map(),
                        value => value.clone(),
                    },
                }
            } else {
                EvaluatedProduction::Static
            },
            target,
            fingerprint: Some(fingerprint),
            owner: owner.clone(),
            declared_at: location.trace.clone(),
        });
        matched.push(relative);
    }
    if !skipped.is_empty() {
        if !set_selector {
            return Err(WombatError::configuration(format!(
                "unallocated artifact source `{source_path}` requires an explicit `to`"
            )));
        }
        match state.artifact_policy.unallocated {
            crate::manifest::UnallocatedPolicy::Ignore => {}
            crate::manifest::UnallocatedPolicy::Warn => {
                state.artifact_notices.push(ArtifactNotice {
                    kind: ArtifactNoticeKind::UnallocatedSkipped,
                    owner: owner.clone(),
                    selector: source_path.to_string(),
                    skipped: skipped.clone(),
                    declared_at: location.trace.clone(),
                })
            }
            crate::manifest::UnallocatedPolicy::Error => {
                return Err(WombatError::configuration(format!(
                    "artifact selector `{source_path}` contains unallocated children without an explicit `to`"
                )));
            }
        }
    }
    if set_selector && matched.is_empty() && !allow_empty {
        return Err(WombatError::configuration(format!(
            "artifact selector `{source_path}` produced no allocated files after exclusions and source policy; set `allow_empty = true` if this is intentional"
        )));
    }
    let selection_kind = if selector.glob {
        ArtifactSelectionKind::Glob
    } else if set_selector {
        ArtifactSelectionKind::Directory
    } else {
        ArtifactSelectionKind::Exact
    };
    state.artifact_selections.push(ArtifactSelection {
        owner: owner.clone(),
        declared: source_path.to_string(),
        expanded: selector.expanded.clone(),
        physical: selector.physical.clone(),
        source_base: display_path(&repository_root, &source_base),
        source_base_logical: base_logical,
        source_base_target: base_target.clone(),
        source_base_hidden: base_hidden,
        hidden,
        kind: selection_kind,
        static_root: selector.static_root.clone(),
        exclusions: exclusions.clone(),
        allow_empty,
        explicit_target: explicit_target.map(str::to_string),
        matches: matched,
        skipped_unallocated: skipped,
        declared_at: location.trace.clone(),
    });
    if set_selector {
        let snapshot = selected_snapshot.expect("set selectors record a traversal snapshot");
        let target_root = match explicit_root {
            Some(root) => Some(root),
            None => base_target
                .as_deref()
                .map(|target| infer_target_root(target, format!("selector:{source_path}")))
                .transpose()?,
        };
        state.directories.push(EvaluatedDirectory {
            declared_source: source_path.to_string(),
            root: display_path(&repository_root, &selected_snapshot_root),
            physical_selector: selector.physical,
            static_root: selector.static_root,
            hidden,
            glob: selector.glob,
            exclusions,
            target_root,
            owner,
            declared_at: location.trace,
            snapshot,
        });
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
    let mut candidates = Vec::new();
    collect_module_files(&root.join("modules"), &mut candidates)?;
    let matches = candidates
        .iter()
        .filter(|file| {
            file.extension().is_some_and(|ext| ext == "lua")
                && file.file_stem().is_some_and(|stem| stem == name)
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [file] => Ok(ModuleLocation {
            file: (*file).clone(),
        }),
        [] => Err(WombatError::configuration(format!(
            "module `{name}` was not found beneath `modules/`"
        ))),
        _ => {
            let found = matches
                .iter()
                .map(|file| display_path(root, file))
                .collect::<Vec<_>>()
                .join(", ");
            Err(WombatError::configuration(format!(
                "module id `{name}` is duplicated by filename stem: {found}"
            )))
        }
    }
}

fn collect_module_files(directory: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(WombatError::io(directory, error)),
    };
    let mut entries = entries
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|error| WombatError::io(directory, error))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let metadata =
            fs::symlink_metadata(&path).map_err(|error| WombatError::io(&path, error))?;
        if metadata.file_type().is_symlink() {
            return Err(WombatError::configuration(format!(
                "module path `{}` must not be a symbolic link",
                path.display()
            )));
        }
        if metadata.is_dir() {
            collect_module_files(&path, files)?;
        } else if metadata.is_file() {
            if path.extension().is_some_and(|ext| ext == "lua") {
                files.push(path);
            }
        } else {
            return Err(WombatError::configuration(format!(
                "module path `{}` must be a regular file or directory",
                path.display()
            )));
        }
    }
    Ok(())
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
    preparations: Vec<ProviderPreparation>,
) -> Result<EvaluatedManifest> {
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
            source_base: module.source_base.clone(),
        })
        .collect();
    let dependencies = state.dependencies.iter().cloned().collect();
    let mut artifacts = state.artifacts.clone();
    artifacts.sort_by(|left, right| {
        left.target
            .key()
            .cmp(right.target.key())
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

    let ladder = state.ladder.clone().unwrap_or_default();
    validate_ladder_actions(&ladder, &state.requirements, &state.tasks, &state.scripts)?;
    let requirements = normalize_requirements(state.requirements.clone(), &ladder)?;
    let project_identity = digest_bytes(state.root.to_string_lossy().as_bytes());
    Ok(EvaluatedManifest {
        plan_id: String::new(),
        project_arguments: state
            .project_arguments
            .iter()
            .map(|argument| {
                argument.to_str().map(str::to_owned).ok_or_else(|| {
                    WombatError::configuration("project arguments must be valid UTF-8")
                })
            })
            .collect::<Result<Vec<_>>>()?,
        sources: state
            .sources
            .values()
            .map(|source| source.manifest.clone())
            .collect(),
        inputs: state.inputs.clone(),
        target: state.target.clone(),
        observations: state.observations.values().cloned().collect(),
        process_observations: state.process_observations.clone(),
        modules,
        dependencies,
        project_identity,
        ladder,
        providers: state.providers.clone(),
        requirements,
        preparations,
        tasks: state.tasks.clone(),
        scripts: state.scripts.clone(),
        script_outcomes: Vec::new(),
        artifact_policy: state.artifact_policy,
        artifact_notices: state.artifact_notices.clone(),
        artifact_selections: state.artifact_selections.clone(),
        artifacts,
        directories,
    })
}

fn normalize_requirements(
    mut requirements: Vec<Requirement>,
    ladder: &ExecutionLadder,
) -> Result<Vec<Requirement>> {
    requirements.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| {
                left.candidates[left.selected as usize]
                    .name()
                    .cmp(right.candidates[right.selected as usize].name())
            })
            .then_with(|| left.binding.provider.cmp(&right.binding.provider))
            .then_with(|| left.binding.identity.cmp(&right.binding.identity))
            .then_with(|| left.declared_at.cmp(&right.declared_at))
    });
    let mut normalized: Vec<Requirement> = Vec::new();
    for requirement in requirements {
        let same = normalized.last().is_some_and(|previous| {
            previous.kind == requirement.kind
                && previous.candidates[previous.selected as usize].name()
                    == requirement.candidates[requirement.selected as usize].name()
                && previous.binding.provider == requirement.binding.provider
                && previous.binding.identity == requirement.binding.identity
        });
        if same {
            let previous = normalized.last_mut().expect("same requirement exists");
            if previous.candidates != requirement.candidates
                || previous.choice != requirement.choice
            {
                return Err(WombatError::configuration(format!(
                    "conflicting requirement declarations for {} through `{}` at {} and {}",
                    requirement.candidates[requirement.selected as usize].name(),
                    requirement.binding.provider,
                    previous.declared_at,
                    requirement.declared_at,
                )));
            }
            if ladder.position(&requirement.when) < ladder.position(&previous.when) {
                previous.when = requirement.when;
            }
        } else {
            normalized.push(requirement);
        }
    }
    Ok(normalized)
}

fn validate_ladder_actions(
    ladder: &ExecutionLadder,
    requirements: &[Requirement],
    tasks: &[EvaluatedTask],
    scripts: &[Script],
) -> Result<()> {
    let mut used = BTreeSet::new();
    for (kind, id, location) in requirements
        .iter()
        .map(|value| ("requirement", &value.when, &value.declared_at))
        .chain(
            tasks
                .iter()
                .map(|value| ("task", &value.task.at, &value.task.declared_at)),
        )
        .chain(
            scripts
                .iter()
                .map(|value| ("script", &value.at, &value.declared_at)),
        )
    {
        if !ladder.contains(id) {
            return Err(WombatError::configuration(format!(
                "{kind} targets unknown rung `{id}` at {location}"
            )));
        }
        if ladder.is_container(id) {
            return Err(WombatError::configuration(format!(
                "{kind} cannot target container rung `{id}` at {location}"
            )));
        }
        used.insert(id.clone());
    }
    let first_task: RungId = CoreRung::MaterialiseBefore.into();
    let last_task: RungId = CoreRung::MaterialiseArtifacts.into();
    let first = ladder.position(&first_task).expect("fixed rung exists");
    let last = ladder.position(&last_task).expect("fixed rung exists");
    for task in tasks {
        let position = ladder
            .position(&task.task.at)
            .expect("task rung was validated");
        if position < first || position > last {
            return Err(WombatError::configuration(format!(
                "task `{}` rung `{}` must be between materialise.before and materialise.artifacts",
                task.task.identity, task.task.at
            )));
        }
    }
    for rung in &ladder.flattened {
        if rung.core.is_none() && !ladder.is_container(&rung.id) && !used.contains(&rung.id) {
            return Err(WombatError::configuration(format!(
                "custom leaf rung `{}` has no actions",
                rung.id
            )));
        }
    }
    Ok(())
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
            .cmp(right.target.key())
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
                &artifact.target.path,
                "multiple artifacts resolve to the same target",
                &duplicates,
            ));
        }

        let descendants = ordered
            .iter()
            .skip(index + 1)
            .filter(|descendant| is_path_ancestor(&artifact.target.path, &descendant.target.path))
            .copied()
            .collect::<Vec<_>>();
        if !descendants.is_empty() {
            let displays = descendants
                .iter()
                .map(|descendant| format!("`{}`", descendant.target.path))
                .collect::<Vec<_>>()
                .join(", ");
            let mut conflicts = Vec::with_capacity(descendants.len() + 1);
            conflicts.push(*artifact);
            conflicts.extend(descendants);
            return Err(artifact_conflict(
                &artifact.target.path,
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
                SourceOrigin::Direct { declared, .. } => {
                    format!("`{}` (direct source `{declared}`)", artifact.source)
                }
                SourceOrigin::Directory {
                    declared,
                    root,
                    relative,
                    ..
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
