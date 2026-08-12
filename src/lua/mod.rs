use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::time::Duration;

use globset::{Glob, GlobSetBuilder};
use mlua::{Function, Lua, LuaOptions, MultiValue, StdLib, Table, Value};
use sha2::{Digest, Sha256};

use crate::execution::ladder::{CoreRung, ExecutionLadder, LadderRung, RungId};
use crate::model::context::{HostContext, ResolvedTarget, TargetOrigin, TargetPlatform};
use crate::model::frozen::FrozenValue;
use crate::model::manifest::{
    ArtifactKind, ArtifactNotice, ArtifactNoticeKind, ArtifactPolicy, ArtifactSelection,
    ArtifactSelectionKind, BuildInput, Dependency, DependencyKind, EvaluatedArtifact,
    EvaluatedDirectory, EvaluatedManifest, EvaluatedProduction, EvaluatedTask, InterpreterFamily,
    MAX_SOURCE_TRACE_FRAMES, ManifestModule, ModuleSourceBase, Observation, ObservationSubject,
    ProcessEnvironmentChange, ProcessInvocation, ProcessObservation, Provider, ProviderBinding,
    ProviderOrigin, ProviderPreparation, Publications, Requirement, RequirementCandidate,
    RequirementChoice, RequirementKind, ResolutionAttempt, ResolutionOutcome, Script,
    ScriptPayload, ScriptSchedule, ScriptScope, SourceFile, SourceLocation, SourceOrigin,
    SourceTrace, Task, TaskCachePolicy, TaskLogPolicy, TaskRunner, TaskTargetRoot,
};
use crate::model::path::{
    infer_target, infer_target_root, parse_explicit_target, parse_explicit_target_root,
    reject_legacy_artifact_trees, validate_relative_path,
};
use crate::model::selection::{
    compile_selector, hidden_components_authorized, in_static_scope, is_excluded, matcher,
    project_physical,
};
use crate::model::source::{
    SourceFingerprint, fingerprint_regular_file, snapshot_directory_filtered,
    validate_source_components,
};
use crate::project::inputs::{self, InputSpec};
use crate::{Diagnostic, Result, WombatError};

mod actions;
mod api;
mod artifacts;
mod finalize;
mod loading;
mod modules;
mod requirements;

use actions::*;
use api::*;
use artifacts::*;
pub(crate) use finalize::validate_artifact_conflicts;
use finalize::*;
use loading::*;
use modules::*;
use requirements::*;

const WOMBAT_LUA: &str = include_str!("../../lua/wombat/init.lua");
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
