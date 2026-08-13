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

use crate::lua::{EvaluationOptions, EvaluationOutcome, evaluate_with};
use crate::model::context::HostContext;
use crate::model::manifest::{
    Artifact, EvaluatedArtifact, EvaluatedDirectory, EvaluatedProduction, FileContent,
    MANIFEST_FORMAT_VERSION, Manifest, Production, RendererIdentity, SourceOrigin, TargetOrigin,
};
use crate::model::path::{
    display_target, expand_target_root, parse_explicit_target, parse_explicit_target_root,
    validate_declared_source, validate_relative_path,
};
use crate::model::source::{
    SourceFingerprint, fingerprint_regular_file, snapshot_directory_filtered,
};
use crate::{Result, WombatError};

pub(crate) mod cache;
mod materialisation;
mod publication;
mod validation;

use materialisation::{
    executable_intent, materialise_product, revalidate_sources, write_json_atomic,
};
use publication::{
    clear_directory_contents, ensure_plain_directory, ensure_plain_file,
    ensure_plain_file_or_missing, inspect_product, publish, recover_publication,
};
use validation::verify_product;
pub(crate) use validation::{validate_artifact_metadata, validate_manifest};

fn digest_string(bytes: impl AsRef<[u8]>) -> String {
    crate::storage::digest::prefixed_hex(bytes)
}

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
    pub log_level: Option<crate::presentation::LogLevel>,
    pub log_adjustment: i8,
    pub compile_only: bool,
    pub yes: bool,
    pub clean: bool,
    pub reconcile_requirements: bool,
    pub requirement_boundary: crate::execution::ladder::CoreRung,
    pub rerun_scripts: bool,
    pub allow_host_scripts: bool,
    pub script_state_root: Option<PathBuf>,
    #[doc(hidden)]
    pub task_interpreters: BTreeMap<String, crate::model::manifest::TaskRunner>,
}

impl BuildOptions {
    pub fn new(source_root: impl Into<PathBuf>, build_dir: impl Into<PathBuf>) -> Self {
        Self {
            source_root: source_root.into(),
            build_dir: build_dir.into(),
            project_arguments: Vec::new(),
            host: None,
            log_level: None,
            log_adjustment: 0,
            compile_only: false,
            yes: false,
            clean: false,
            reconcile_requirements: false,
            requirement_boundary: crate::execution::ladder::CoreRung::MaterialiseAfter,
            rerun_scripts: false,
            allow_host_scripts: false,
            script_state_root: None,
            task_interpreters: BTreeMap::new(),
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

    pub fn with_log_level(mut self, log_level: crate::presentation::LogLevel) -> Self {
        self.log_level = Some(log_level);
        self
    }

    pub fn with_log_adjustment(mut self, adjustment: i8) -> Self {
        self.log_adjustment = adjustment;
        self
    }

    pub fn with_compile_only(mut self, compile_only: bool) -> Self {
        self.compile_only = compile_only;
        self
    }

    pub fn with_yes(mut self, yes: bool) -> Self {
        self.yes = yes;
        self
    }

    pub fn with_clean(mut self, clean: bool) -> Self {
        self.clean = clean;
        self
    }

    /// Root workflows opt into provider reconciliation. Library callers can
    /// construct exact products for inspection without mutating the host.
    pub fn with_provider_reconciliation(mut self, reconcile: bool) -> Self {
        self.reconcile_requirements = reconcile;
        self
    }

    #[doc(hidden)]
    pub fn with_requirement_boundary(
        mut self,
        boundary: crate::execution::ladder::CoreRung,
    ) -> Self {
        self.requirement_boundary = boundary;
        self
    }

    pub fn with_rerun_scripts(mut self, rerun: bool) -> Self {
        self.rerun_scripts = rerun;
        self
    }

    pub fn with_allow_host_scripts(mut self, allow: bool) -> Self {
        self.allow_host_scripts = allow;
        self
    }

    #[doc(hidden)]
    pub fn with_script_state_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.script_state_root = Some(root.into());
        self
    }

    #[doc(hidden)]
    pub fn with_task_interpreters(
        mut self,
        interpreters: BTreeMap<String, crate::model::manifest::TaskRunner>,
    ) -> Self {
        self.task_interpreters = interpreters;
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuildStatus {
    Created,
    Updated,
    Unchanged,
    Reused,
    Repaired,
}

impl fmt::Display for BuildStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Created => "created",
            Self::Updated => "updated",
            Self::Unchanged => "unchanged",
            Self::Reused => "reused",
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
    #[doc(hidden)]
    pub requirement_authorization: Option<crate::requirements::RequirementAuthorization>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlanOutcome {
    pub build_dir: PathBuf,
    pub plan: crate::model::manifest::BuildPlan,
}

/// Result of executing an already-constructed plan.  This deliberately carries
/// no evaluated Lua state: callers can only materialise the private plan bundle.
pub type MaterialiseOutcome = BuildOutcome;

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
    construction_version: u32,
    plan_id: &'a str,
    sources: &'a [crate::model::manifest::SourceFile],
    inputs: &'a [crate::model::manifest::BuildInput],
    target: &'a crate::model::context::ResolvedTarget,
    observations: &'a [crate::model::manifest::Observation],
    process_observations: &'a [crate::model::manifest::ProcessObservation],
    modules: &'a [crate::model::manifest::ManifestModule],
    dependencies: &'a [crate::model::manifest::Dependency],
    ladder: &'a crate::execution::ladder::ExecutionLadder,
    providers: &'a [crate::model::manifest::Provider],
    requirements: &'a [crate::model::manifest::Requirement],
    preparations: &'a [crate::model::manifest::ProviderPreparation],
    tasks: &'a [crate::model::manifest::Task],
    scripts: &'a [crate::model::manifest::Script],
    artifact_policy: &'a crate::model::manifest::ArtifactPolicy,
    artifact_notices: &'a [crate::model::manifest::ArtifactNotice],
    artifact_selections: &'a [crate::model::manifest::ArtifactSelection],
    artifacts: &'a [Artifact],
}

enum CurrentProduct {
    Missing,
    Valid(Box<Manifest>),
    Invalid,
}

pub fn build(options: BuildOptions) -> Result<BuildOutcome> {
    if let Some(reused) = try_reuse_product(&options)? {
        return Ok(reused);
    }
    let planned = plan_or_reuse(options.clone())?;
    materialise_at(options, planned.build_dir)
}

fn try_reuse_product(options: &BuildOptions) -> Result<Option<BuildOutcome>> {
    if !options.reconcile_requirements
        || options.clean
        || !crate::project::workflow_policy(&options.source_root)?.reuse
    {
        return Ok(None);
    }
    let Some((_, build_dir, plan, _)) = reusable_stored_plan(options)? else {
        return Ok(None);
    };
    if plan.tasks.iter().any(|task| !task.cache.enabled) {
        return Ok(None);
    }
    let lock_path = build_dir.join(".wombat/lock");
    ensure_plain_file(&lock_path)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| WombatError::io(&lock_path, error))?;
    acquire_exclusive(&lock, &build_dir)?;
    let result = (|| {
        let manifest = match verify_product(&build_dir) {
            Ok(manifest) => manifest,
            Err(_) => return Ok(None),
        };
        if manifest.plan_id != plan.plan_id {
            return Ok(None);
        }
        let execution_mode = if options.compile_only {
            crate::model::manifest::ExecutionMode::CompileOnly
        } else {
            crate::model::manifest::ExecutionMode::Normal
        };
        if manifest.execution_mode != execution_mode {
            return Ok(None);
        }
        let mut authorization = None;
        if options.reconcile_requirements && !options.compile_only && !plan.requirements.is_empty()
        {
            authorization = Some(crate::requirements::authorize_target_plan_until(
                &build_dir,
                &plan,
                options.requirement_boundary,
                options.yes,
            )?);
        }
        let state_root = options
            .script_state_root
            .clone()
            .map_or_else(crate::execution::script::materialise_state_root, Ok)?;
        let mut journal = crate::execution::ladder::ExecutionJournal::new_for_ladder(
            plan.plan_id.clone(),
            crate::execution::ladder::CoreRung::MaterialiseAfter,
            &plan.ladder,
        );
        journal.configure(execution_mode, Vec::new());
        journal.build_id = Some(manifest.build_id.clone());
        journal.record_reuse("product");
        for rung in crate::execution::runner::ExecutionRange::through(
            &plan.ladder,
            crate::execution::ladder::CoreRung::MaterialiseAfter,
        )? {
            journal.set_id(&rung, crate::execution::ladder::ExecutionStatus::Running);
            if let Some(approved) = &mut authorization {
                crate::requirements::prepare_target_plan_at_authorized(
                    &build_dir, &plan, &rung, approved,
                )?;
            }
            let scripts = plan
                .scripts
                .iter()
                .filter(|script| script.at == rung)
                .cloned()
                .collect::<Vec<_>>();
            crate::execution::script::check_runners(&scripts)?;
            let outcomes = crate::execution::script::execute_at(
                &plan.scripts,
                &rung,
                &crate::execution::script::ScriptExecutionOptions {
                    state_root: &state_root,
                    payload_root: &build_dir,
                    payload_kind: crate::execution::script::PayloadKind::Product,
                    project_identity: &plan.project_identity,
                    plan_id: &plan.plan_id,
                    build_id: Some(&manifest.build_id),
                    execution_mode,
                    allow_host_scripts: options.allow_host_scripts,
                    rerun: options.rerun_scripts,
                    target_root: None,
                },
            )?;
            for outcome in &outcomes {
                journal.record_action(
                    &outcome.identity,
                    &rung,
                    match outcome.status {
                        crate::model::manifest::ScriptOutcomeStatus::Ran => {
                            crate::execution::ladder::ExecutionStatus::Succeeded
                        }
                        _ => crate::execution::ladder::ExecutionStatus::Skipped,
                    },
                    &outcome.reason,
                );
            }
            journal.set_id(
                &rung,
                if rung.core() == Some(crate::execution::ladder::CoreRung::MaterialisePublish) {
                    crate::execution::ladder::ExecutionStatus::Reused
                } else {
                    crate::execution::ladder::ExecutionStatus::Succeeded
                },
            );
        }
        crate::execution::ladder::write(&build_dir, &journal)?;
        let mut reused = outcome(BuildStatus::Reused, build_dir.clone(), manifest);
        if options.requirement_boundary > crate::execution::ladder::CoreRung::MaterialiseAfter {
            reused.requirement_authorization = authorization;
        }
        Ok(Some(reused))
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

fn reusable_stored_plan(
    options: &BuildOptions,
) -> Result<
    Option<(
        PathBuf,
        PathBuf,
        crate::model::manifest::BuildPlan,
        crate::model::manifest::EvaluatedManifest,
    )>,
> {
    if options.clean || !crate::project::workflow_policy(&options.source_root)?.reuse {
        return Ok(None);
    }
    let source_root = match fs::canonicalize(&options.source_root) {
        Ok(root) => root,
        Err(_) => return Ok(None),
    };
    let requested = if options.build_dir.is_absolute() {
        options.build_dir.clone()
    } else {
        source_root.join(&options.build_dir)
    };
    let build_dir = match resolve_maybe_missing(&requested) {
        Ok(path) => path,
        Err(_) => return Ok(None),
    };
    let plan = match crate::model::plan::read(&build_dir) {
        Ok(plan) => plan,
        Err(_) => return Ok(None),
    };
    let desired = match crate::model::plan::read_execution(&build_dir, &plan) {
        Ok(desired) => desired,
        Err(_) => return Ok(None),
    };
    let arguments = options
        .project_arguments
        .iter()
        .map(|argument| argument.to_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>();
    if arguments.as_deref() != Some(plan.project_arguments.as_slice()) {
        return Ok(None);
    }
    let plan_path = build_dir.join(".wombat/plan/plan.json");
    let age = match fs::metadata(&plan_path).and_then(|metadata| metadata.modified()) {
        Ok(modified) => modified.elapsed().unwrap_or(std::time::Duration::MAX),
        Err(_) => return Ok(None),
    };
    if age > crate::project::workflow_policy(&source_root)?.freshness {
        return Ok(None);
    }
    if validate_stored_plan_closure(&source_root, &plan, &desired).is_err() {
        return Ok(None);
    }
    let host = options.host.clone().map_or_else(HostContext::observe, Ok)?;
    if !observed_host_facts_match(&plan.observations, &host) {
        return Ok(None);
    }
    Ok(Some((source_root, build_dir, plan, desired)))
}

/// Reuse a fresh stored plan when its complete observed closure still matches;
/// otherwise construct and persist a new plan.
pub fn plan_or_reuse(options: BuildOptions) -> Result<PlanOutcome> {
    if let Some((_, build_dir, plan, _)) = reusable_stored_plan(&options)? {
        return Ok(PlanOutcome { build_dir, plan });
    }
    plan(options)
}

/// Materialise the exact plan previously written beneath `build_dir`.
/// Configuration Lua is never evaluated by this operation.
pub fn materialise(options: BuildOptions) -> Result<MaterialiseOutcome> {
    materialise_at(options.clone(), options.build_dir.clone())
}

fn materialise_at(options: BuildOptions, requested_build_dir: PathBuf) -> Result<BuildOutcome> {
    let source_root = fs::canonicalize(&options.source_root)
        .map_err(|error| WombatError::io(&options.source_root, error))?;
    let requested_build = if requested_build_dir.is_absolute() {
        requested_build_dir
    } else {
        source_root.join(requested_build_dir)
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
        if options.clean {
            clean_transient_workspace(&build_dir)?;
        }

        let plan = crate::model::plan::read(&build_dir)?;
        let mut journal = crate::execution::ladder::read(&build_dir)
            .map(|journal| {
                journal.reopen_for_ladder(
                    &plan.plan_id,
                    crate::execution::ladder::CoreRung::MaterialiseAfter,
                    &plan.ladder,
                )
            })
            .unwrap_or_else(|_| {
                crate::execution::ladder::ExecutionJournal::new_for_ladder(
                    plan.plan_id.clone(),
                    crate::execution::ladder::CoreRung::MaterialiseAfter,
                    &plan.ladder,
                )
            });
        let desired = crate::model::plan::read_execution(&build_dir, &plan)?;
        validate_stored_plan_closure(&source_root, &plan, &desired)?;
        let host = options.host.clone().map_or_else(HostContext::observe, Ok)?;
        if !options.compile_only && !plan.target.platform.locally_compatible_with(&host.platform) {
            return Err(WombatError::configuration(format!(
                "target {} is not compatible with this host {}; use --compile-only to materialise without local bring-up",
                plan.target.platform.compact(),
                host.platform.compact()
            )));
        }
        let skipped_requirement_gates = if options.compile_only {
            plan.requirements
                .iter()
                .filter(|requirement| {
                    plan.ladder.before_or_at(
                        &requirement.when,
                        crate::execution::ladder::CoreRung::MaterialiseAfter,
                    )
                })
                .map(|requirement| requirement.when.id().to_string())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        journal.configure(
            if options.compile_only {
                crate::model::manifest::ExecutionMode::CompileOnly
            } else {
                crate::model::manifest::ExecutionMode::Normal
            },
            skipped_requirement_gates.clone(),
        );
        crate::execution::ladder::write(&build_dir, &journal)?;
        let mut authorization = if !options.compile_only
            && options.reconcile_requirements
            && !plan.requirements.is_empty()
        {
            Some(crate::requirements::authorize_target_plan_until(
                &build_dir,
                &plan,
                options.requirement_boundary,
                options.yes,
            )?)
        } else {
            None
        };
        let execution_mode = if options.compile_only {
            crate::model::manifest::ExecutionMode::CompileOnly
        } else {
            crate::model::manifest::ExecutionMode::Normal
        };
        let script_state_root = options
            .script_state_root
            .clone()
            .map_or_else(crate::execution::script::materialise_state_root, Ok)?;
        let mut desired = Some(desired);
        let mut staging = None;
        let mut materialised_manifest = None;
        let mut published_manifest = None;
        let mut status = None;
        for rung in crate::execution::runner::ExecutionRange::through(
            &plan.ladder,
            crate::execution::ladder::CoreRung::MaterialiseAfter,
        )? {
            if let Some(authorization) = &mut authorization {
                let gate = format!("requirements:{}", rung.id());
                journal.record_action(
                    &gate,
                    &rung,
                    crate::execution::ladder::ExecutionStatus::Running,
                    "observing deadline gate",
                );
                crate::execution::ladder::write(&build_dir, &journal)?;
                if let Err(error) = crate::requirements::prepare_target_plan_at_authorized(
                    &build_dir,
                    &plan,
                    &rung,
                    authorization,
                ) {
                    journal.record_action(
                        &gate,
                        &rung,
                        crate::execution::ladder::ExecutionStatus::Failed,
                        error.to_string(),
                    );
                    journal.fail_id(&rung, &error);
                    let _ = crate::execution::ladder::write(&build_dir, &journal);
                    return Err(error);
                }
                journal.record_action(
                    gate,
                    &rung,
                    crate::execution::ladder::ExecutionStatus::Succeeded,
                    "requirements observed",
                );
            }
            journal.set_id(&rung, crate::execution::ladder::ExecutionStatus::Running);
            crate::execution::ladder::write(&build_dir, &journal)?;

            let mut actions = plan
                .tasks
                .iter()
                .filter(|task| task.at == rung)
                .map(|task| (task.declaration_order, false, task.identity.clone()))
                .chain(
                    plan.scripts
                        .iter()
                        .filter(|script| script.at == rung)
                        .map(|script| (script.declaration_order, true, script.identity.clone())),
                )
                .collect::<Vec<_>>();
            actions.sort_by_key(|(order, _, _)| *order);
            for (_, script_action, identity) in actions {
                journal.record_action(
                    &identity,
                    &rung,
                    crate::execution::ladder::ExecutionStatus::Running,
                    "executing",
                );
                crate::execution::ladder::write(&build_dir, &journal)?;
                let action_result = if script_action {
                    let script = plan
                        .scripts
                        .iter()
                        .find(|script| script.identity == identity)
                        .expect("planned script exists");
                    crate::execution::script::check_runners(std::slice::from_ref(script))?;
                    crate::execution::script::execute_at(
                        std::slice::from_ref(script),
                        &rung,
                        &crate::execution::script::ScriptExecutionOptions {
                            state_root: &script_state_root,
                            payload_root: &build_dir.join(".wombat/plan"),
                            payload_kind: crate::execution::script::PayloadKind::Plan,
                            project_identity: &plan.project_identity,
                            plan_id: &plan.plan_id,
                            build_id: materialised_manifest
                                .as_ref()
                                .map(|manifest: &Manifest| manifest.build_id.as_str()),
                            execution_mode,
                            allow_host_scripts: options.allow_host_scripts,
                            rerun: options.rerun_scripts,
                            target_root: None,
                        },
                    )
                    .map(|outcomes| {
                        let status = outcomes.first().map_or(
                            crate::execution::ladder::ExecutionStatus::Succeeded,
                            |outcome| match outcome.status {
                                crate::model::manifest::ScriptOutcomeStatus::Ran => {
                                    crate::execution::ladder::ExecutionStatus::Succeeded
                                }
                                crate::model::manifest::ScriptOutcomeStatus::ScheduledSkip
                                | crate::model::manifest::ScriptOutcomeStatus::CompileOnlySkip
                                | crate::model::manifest::ScriptOutcomeStatus::Refused => {
                                    crate::execution::ladder::ExecutionStatus::Skipped
                                }
                            },
                        );
                        let reason = outcomes.first().map_or_else(
                            || "completed".to_string(),
                            |outcome| outcome.reason.clone(),
                        );
                        (status, reason)
                    })
                } else {
                    let task = plan
                        .tasks
                        .iter()
                        .find(|task| task.identity == identity)
                        .expect("planned task exists");
                    crate::execution::task::check_runners(std::slice::from_ref(task))?;
                    crate::execution::task::execute_task(
                        &source_root,
                        &build_dir,
                        desired.as_mut().ok_or_else(|| {
                            WombatError::configuration(
                                "task was ordered after artifact materialisation",
                            )
                        })?,
                        &rung,
                        &identity,
                    )
                    .map(|()| {
                        (
                            crate::execution::ladder::ExecutionStatus::Succeeded,
                            "completed".to_string(),
                        )
                    })
                };
                match action_result {
                    Ok((action_status, reason)) => {
                        journal.record_action(&identity, &rung, action_status, reason);
                        crate::execution::ladder::write(&build_dir, &journal)?;
                    }
                    Err(error) => {
                        journal.record_action(
                            &identity,
                            &rung,
                            crate::execution::ladder::ExecutionStatus::Failed,
                            error.to_string(),
                        );
                        journal.fail_id(&rung, &error);
                        let _ = crate::execution::ladder::write(&build_dir, &journal);
                        return Err(error);
                    }
                }
            }

            match rung.core() {
                Some(crate::execution::ladder::CoreRung::MaterialiseArtifacts) => {
                    let staging_root = internal.join("staging");
                    ensure_plain_directory(&staging_root)?;
                    clear_directory_contents(&staging_root)?;
                    let next_staging = Builder::new()
                        .prefix("build-")
                        .tempdir_in(&staging_root)
                        .map_err(|error| WombatError::io(&staging_root, error))?;
                    let cache = crate::build::cache::BuildCache::open(&build_dir)?;
                    let manifest = materialise_product(
                        &source_root,
                        next_staging.path(),
                        desired.take().expect("artifacts materialise once"),
                        &cache,
                        execution_mode,
                        skipped_requirement_gates.clone(),
                    )?;
                    staging = Some(next_staging);
                    materialised_manifest = Some(manifest);
                }
                Some(crate::execution::ladder::CoreRung::MaterialisePublish) => {
                    let staging = staging.as_ref().expect("artifacts precede publication");
                    let manifest = materialised_manifest.as_ref().expect("manifest exists");
                    let staged = verify_product(staging.path())?;
                    debug_assert_eq!(&staged, manifest);
                    let current = inspect_product(&build_dir);
                    if let CurrentProduct::Valid(existing) = &current
                        && existing.build_id == manifest.build_id
                    {
                        status = Some(BuildStatus::Unchanged);
                        journal.set_id(&rung, crate::execution::ladder::ExecutionStatus::Reused);
                        journal.build_id = Some(existing.build_id.clone());
                        journal.record_reuse("product");
                        published_manifest = Some(existing.as_ref().clone());
                    } else {
                        status = Some(match current {
                            CurrentProduct::Missing => BuildStatus::Created,
                            CurrentProduct::Valid(_) => BuildStatus::Updated,
                            CurrentProduct::Invalid => BuildStatus::Repaired,
                        });
                        publish(&build_dir, staging.path())?;
                        let published = verify_product(&build_dir)?;
                        journal.build_id = Some(published.build_id.clone());
                        published_manifest = Some(published);
                    }
                }
                _ => {}
            }
            if !matches!(
                journal.rungs.iter().find(|(id, _)| id == &rung),
                Some((_, crate::execution::ladder::ExecutionStatus::Reused))
            ) {
                journal.set_id(&rung, crate::execution::ladder::ExecutionStatus::Succeeded);
            }
            crate::execution::ladder::write(&build_dir, &journal)?;
        }

        let published = published_manifest.expect("publication rung produced a manifest");
        let mut outcome = outcome(
            status.expect("publication determines status"),
            build_dir.clone(),
            published,
        );
        if options.requirement_boundary > crate::execution::ladder::CoreRung::MaterialiseAfter {
            outcome.requirement_authorization = authorization;
        }
        Ok(outcome)
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

fn clean_transient_workspace(build_dir: &Path) -> Result<()> {
    let internal = build_dir.join(".wombat");
    for name in ["cache", "tasks", "logs", "staging"] {
        let path = internal.join(name);
        if path.exists() {
            clear_directory_contents(&path)?;
        }
    }
    let journal = internal.join("execution-journal.json");
    if journal.exists() {
        fs::remove_file(&journal).map_err(|error| WombatError::io(&journal, error))?;
    }
    Ok(())
}

fn validate_stored_plan_closure(
    source_root: &Path,
    plan: &crate::model::manifest::BuildPlan,
    desired: &crate::model::manifest::EvaluatedManifest,
) -> Result<()> {
    if plan.plan_id != desired.plan_id {
        return Err(WombatError::configuration(
            "stored plan execution payload does not match plan identity",
        ));
    }
    for source in &plan.sources {
        let path = source_root.join(&source.path);
        let bytes = fs::read(&path).map_err(|error| WombatError::io(&path, error))?;
        if digest_string(Sha256::digest(&bytes)) != source.digest {
            return Err(WombatError::configuration(format!(
                "stored plan is stale because source `{}` changed; run `wombat plan construct`",
                source.path
            )));
        }
    }
    for artifact in &plan.artifacts {
        use crate::model::manifest::PlannedProduction;
        match &artifact.production {
            PlannedProduction::Static {
                source_digest,
                executable,
            }
            | PlannedProduction::Template {
                source_digest,
                executable,
                ..
            } => {
                let path = source_root.join(&artifact.source);
                let bytes = fs::read(&path).map_err(|error| WombatError::io(&path, error))?;
                if digest_string(Sha256::digest(&bytes)) != *source_digest
                    || executable_intent(
                        &fs::metadata(&path).map_err(|error| WombatError::io(&path, error))?,
                    ) != *executable
                {
                    return Err(WombatError::configuration(format!(
                        "stored plan is stale because artifact source `{}` changed; run `wombat plan construct`",
                        artifact.source
                    )));
                }
            }
            PlannedProduction::GeneratedLua { content_digest, .. } => {
                let Some(evaluated) = desired
                    .artifacts
                    .iter()
                    .find(|candidate| candidate.target == artifact.target)
                else {
                    return Err(WombatError::configuration(
                        "stored plan execution payload omitted a generated artifact",
                    ));
                };
                let crate::model::manifest::EvaluatedProduction::GeneratedLua { content, .. } =
                    &evaluated.production
                else {
                    return Err(WombatError::configuration(
                        "stored plan generated payload changed kind",
                    ));
                };
                if digest_string(Sha256::digest(content)) != *content_digest {
                    return Err(WombatError::configuration(
                        "stored generated payload integrity mismatch",
                    ));
                }
            }
        }
    }
    for script in &plan.scripts {
        for payload in &script.payloads {
            let path = source_root.join(&payload.source);
            let bytes = fs::read(&path).map_err(|error| WombatError::io(&path, error))?;
            let metadata = fs::metadata(&path).map_err(|error| WombatError::io(&path, error))?;
            if digest_string(Sha256::digest(&bytes)) != payload.digest
                || u64::try_from(bytes.len()).ok() != Some(payload.size)
                || executable_intent(&metadata) != payload.executable
            {
                return Err(WombatError::configuration(format!(
                    "stored plan is stale because script payload `{}` changed; run `wombat plan construct`",
                    payload.source
                )));
            }
        }
    }
    revalidate_sources(source_root, &desired.artifacts, &desired.directories)?;
    Ok(())
}

#[doc(hidden)]
pub fn check_compile_only_plan(
    source_root: &Path,
    build_dir: &Path,
    plan: &crate::model::manifest::BuildPlan,
) -> Result<()> {
    let desired = crate::model::plan::read_execution(build_dir, plan)?;
    validate_stored_plan_closure(source_root, plan, &desired)?;
    crate::execution::task::check_runners(&plan.tasks)?;
    crate::execution::script::check_runners(&plan.scripts)
}

#[doc(hidden)]
pub fn check_plan_execution(
    build_dir: &Path,
    plan: &crate::model::manifest::BuildPlan,
) -> Result<()> {
    crate::execution::script::verify_payloads(
        &build_dir.join(".wombat/plan"),
        &plan.scripts,
        crate::execution::script::PayloadKind::Plan,
    )?;
    crate::execution::task::check_runners(&plan.tasks)?;
    crate::execution::script::check_runners(&plan.scripts)
}

fn observed_host_facts_match(
    observations: &[crate::model::manifest::Observation],
    host: &HostContext,
) -> bool {
    let frozen = host.to_frozen();
    observations
        .iter()
        .filter(|observation| {
            observation.subject == crate::model::manifest::ObservationSubject::Host
        })
        .all(|observation| {
            frozen_value_at_path(&frozen, &observation.path) == Some(&observation.value)
        })
}

fn frozen_value_at_path<'a>(
    root: &'a crate::model::frozen::FrozenValue,
    path: &str,
) -> Option<&'a crate::model::frozen::FrozenValue> {
    path.split('.')
        .try_fold(root, |value, component| match value {
            crate::model::frozen::FrozenValue::Map(map) => map.get(component),
            _ => None,
        })
}

pub fn plan(options: BuildOptions) -> Result<PlanOutcome> {
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
                task_interpreters: options.task_interpreters,
                log_level: options.log_level,
                log_adjustment: options.log_adjustment,
            },
        )? {
            EvaluationOutcome::Manifest(manifest) => *manifest,
            EvaluationOutcome::ProjectHelp(_) => {
                return Err(WombatError::configuration(
                    "project help was requested where a build plan was expected",
                ));
            }
        };
        let plan = crate::model::plan::freeze(&source_root, &desired)?;
        let mut desired = desired;
        desired.plan_id = plan.plan_id.clone();
        crate::model::plan::publish(&build_dir, &source_root, &plan, &desired)?;
        Ok(PlanOutcome {
            build_dir: build_dir.clone(),
            plan,
        })
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
    let mut options = BuildOptions::new(source_root, "build");
    options.host = host;
    project_help_with_options(options)
}

#[doc(hidden)]
pub fn project_help_with_options(options: BuildOptions) -> Result<String> {
    let source_root = options.source_root;
    let source_root =
        fs::canonicalize(&source_root).map_err(|error| WombatError::io(&source_root, error))?;
    let host = options.host.map_or_else(HostContext::observe, Ok)?;
    match evaluate_with(
        &source_root,
        EvaluationOptions {
            project_arguments: vec![OsString::from("--help")],
            host,
            task_interpreters: options.task_interpreters,
            log_level: options.log_level,
            log_adjustment: options.log_adjustment,
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
    copy_product_directory(&source.join("tree"), &destination.join("tree"))?;
    let providers = source.join("providers");
    if providers
        .try_exists()
        .map_err(|error| WombatError::io(&providers, error))?
    {
        copy_product_directory(&providers, &destination.join("providers"))?;
    }
    let scripts = source.join("scripts");
    if scripts
        .try_exists()
        .map_err(|error| WombatError::io(&scripts, error))?
    {
        copy_product_directory(&scripts, &destination.join("scripts"))?;
    }
    Ok(())
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
        requirement_authorization: None,
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
        && [
            "modules",
            "lua",
            "tasks",
            "providers",
            "src",
            "home",
            "dot_config",
            "dot_local",
        ]
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
