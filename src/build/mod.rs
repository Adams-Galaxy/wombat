//! Build workflow: workspace ownership, reuse decisions, and the root entry
//! points.
//!
//! A build directory is an owned workspace, not a scratch folder. It carries a
//! marker recording which source it belongs to, so Wombat refuses to reuse a
//! workspace produced from somewhere else rather than mixing two products
//! together. Unrelated files a user put there are left alone.
//!
//! Reuse is decided from content: when the configuration digests match a fresh
//! existing product, the build is reused rather than repeated. That only holds
//! because identity covers everything that could change the output, which is why
//! the identity payload is listed explicitly rather than derived from the whole
//! manifest.
//!
//! The heavy lifting lives in the children: `workspace` owns safe locations and
//! locks, `materialisation` executes the plan, `publication` swaps results,
//! `product` opens stable verified views, `validation` decides what can be
//! trusted, and `cache` avoids repeating derivations.
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;
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
mod product;
mod publication;
mod validation;
mod workspace;

use materialisation::{executable_intent, materialise_product, revalidate_sources};
pub use product::{OpenedBuild, VerifiedBuild, open_build, verify_build};
use publication::{
    clear_directory_contents, ensure_plain_directory, ensure_plain_file,
    ensure_plain_file_or_missing, inspect_product, publish, recover_publication,
};
use validation::verify_product;
pub(crate) use validation::{validate_artifact_metadata, validate_manifest};
use workspace::{
    acquire_build_lock, clean_transient_workspace, ensure_workspace_marker,
    prepare_workspace_directory, resolve_maybe_missing, validate_build_location,
};

fn digest_string(bytes: impl AsRef<[u8]>) -> String {
    crate::storage::digest::prefixed_hex(bytes)
}

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
    pub check_requirements: bool,
    pub requirement_boundary: crate::execution::ladder::CoreRung,
    pub run_scripts: bool,
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
            check_requirements: true,
            requirement_boundary: crate::execution::ladder::CoreRung::MaterialiseAfter,
            run_scripts: true,
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

    /// Opts into installing what the repository declares.
    ///
    /// Off by default so library callers can construct and inspect an exact
    /// product without touching the host. Root workflows turn it on; nothing is
    /// installed until the resulting plan is authorized.
    pub fn with_provider_reconciliation(mut self, reconcile: bool) -> Self {
        self.reconcile_requirements = reconcile;
        self
    }

    /// Opts out of checking requirements (packages, commands) against the host.
    ///
    /// Unlike [`with_provider_reconciliation`](Self::with_provider_reconciliation),
    /// this leaves reuse eligibility untouched — a fresh cached product is still
    /// served without ever consulting a package manager. On by default; pass
    /// `false` for a quick edit-compile-apply loop that shouldn't pay for a
    /// package check on every run.
    pub fn with_check_requirements(mut self, check: bool) -> Self {
        self.check_requirements = check;
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

    /// Opts out of running `w.script` entries entirely.
    ///
    /// `w.build.task` entries are unaffected — they produce artifacts and stay
    /// part of the build regardless. On by default; pass `false` to skip
    /// scripts for a quick edit-compile-apply loop.
    pub fn with_run_scripts(mut self, run: bool) -> Self {
        self.run_scripts = run;
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
/// What a build did, beyond succeeding.
///
/// The distinction matters for reuse: `Unchanged` and `Reused` both mean nothing
/// was rebuilt, but `Reused` means an existing fresh product satisfied the
/// request without even re-running construction.
pub enum BuildStatus {
    /// No product was there before.
    Created,
    /// A product was there, and its content changed.
    Updated,
    /// Rebuilt to the same identity as the existing product.
    Unchanged,
    /// A fresh matching product already existed, so nothing was rebuilt.
    Reused,
    /// The existing product failed verification and was replaced.
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

/// Construct and materialise a product. Does not deploy.
///
/// Reuses an existing fresh product when the configuration closure still
/// matches, so calling this repeatedly is cheap and idempotent.
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
    let _lock = acquire_build_lock(
        lock,
        &lock_path,
        &build_dir,
        crate::storage::locking::Mode::Exclusive,
    )?;
    (|| {
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
        let mut journal = crate::execution::ladder::ExecutionJournal::new_for_ladder(
            plan.plan_id.clone(),
            crate::execution::ladder::CoreRung::MaterialiseAfter,
            &plan.ladder,
        );
        let manual_requirement_skips = if !options.compile_only
            && options.reconcile_requirements
            && !options.check_requirements
        {
            materialise_requirement_gates(&plan)
        } else {
            Vec::new()
        };
        let mut journal_skips = manifest.skipped_requirement_gates.clone();
        journal_skips.extend(manual_requirement_skips.iter().cloned());
        journal_skips.sort();
        journal_skips.dedup();
        journal.configure(execution_mode, journal_skips);
        journal.build_id = Some(manifest.build_id.clone());
        journal.record_reuse("product");
        if !manual_requirement_skips.is_empty() {
            journal.record_action(
                "requirements:check",
                &crate::execution::ladder::CoreRung::MaterialiseBefore.into(),
                crate::execution::ladder::ExecutionStatus::Skipped,
                "requirement checking skipped by --skip-requirements",
            );
        }
        let mut authorization = None;
        if options.reconcile_requirements
            && options.check_requirements
            && !options.compile_only
            && !plan.requirements.is_empty()
        {
            let requirements_gate = crate::execution::ladder::RungId::from(
                crate::execution::ladder::CoreRung::MaterialiseBefore,
            );
            journal.record_action(
                "requirements:check",
                &requirements_gate,
                crate::execution::ladder::ExecutionStatus::Running,
                "checking requirement status",
            );
            let outcome = crate::requirements::authorize_target_plan_until(
                &build_dir,
                &plan,
                options.requirement_boundary,
                options.yes,
            )?;
            journal.record_action(
                "requirements:check",
                &requirements_gate,
                crate::execution::ladder::ExecutionStatus::Succeeded,
                "requirement status checked",
            );
            authorization = Some(outcome);
        }
        let state_root = if options.run_scripts {
            options
                .script_state_root
                .clone()
                .map_or_else(crate::execution::script::materialise_state_root, Ok)?
        } else {
            PathBuf::new()
        };
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
                    run_scripts: options.run_scripts,
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
    })()
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
    // Freshness is a cheap first gate, not the correctness one. A plan older
    // than the window is discarded without inspecting it; a young one still has
    // to prove its whole closure below.
    if age > crate::project::workflow_policy(&source_root)?.freshness {
        return Ok(None);
    }
    // Every source the plan read must still hash the same. This is what makes
    // reuse safe rather than a guess: an edit anywhere in the closure, including
    // files a glob happened to match, means reconstruction.
    if validate_stored_plan_closure(&source_root, &plan, &desired).is_err() {
        return Ok(None);
    }
    // Only the host facts this plan actually consulted are compared. Checking
    // everything would discard plans over an unrelated OS detail; checking
    // nothing would reuse a plan whose conditionals would now go the other way.
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
/// Execute the plan already stored beneath `build_dir`.
///
/// Configuration Lua is never evaluated here — that is the point of the split.
/// What runs is exactly what `plan construct` froze and what `plan inspect`
/// showed, with no opportunity for a decision to be remade.
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
    let _lock = acquire_build_lock(
        lock,
        &lock_path,
        &build_dir,
        crate::storage::locking::Mode::Exclusive,
    )?;
    (|| {
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
        let product_skipped_requirement_gates = if options.compile_only {
            materialise_requirement_gates(&plan)
        } else {
            Vec::new()
        };
        let manual_requirement_skips = if !options.compile_only
            && options.reconcile_requirements
            && !options.check_requirements
        {
            materialise_requirement_gates(&plan)
        } else {
            Vec::new()
        };
        let mut journal_skips = product_skipped_requirement_gates.clone();
        journal_skips.extend(manual_requirement_skips.iter().cloned());
        journal_skips.sort();
        journal_skips.dedup();
        journal.configure(
            if options.compile_only {
                crate::model::manifest::ExecutionMode::CompileOnly
            } else {
                crate::model::manifest::ExecutionMode::Normal
            },
            journal_skips,
        );
        crate::execution::ladder::write(&build_dir, &journal)?;
        let requirements_gate = crate::execution::ladder::RungId::from(
            crate::execution::ladder::CoreRung::MaterialiseBefore,
        );
        if !manual_requirement_skips.is_empty() {
            journal.record_action(
                "requirements:check",
                &requirements_gate,
                crate::execution::ladder::ExecutionStatus::Skipped,
                "requirement checking skipped by --skip-requirements",
            );
            crate::execution::ladder::write(&build_dir, &journal)?;
        }
        let mut authorization = if !options.compile_only
            && options.reconcile_requirements
            && options.check_requirements
            && !plan.requirements.is_empty()
        {
            journal.record_action(
                "requirements:check",
                &requirements_gate,
                crate::execution::ladder::ExecutionStatus::Running,
                "checking requirement status",
            );
            let outcome = crate::requirements::authorize_target_plan_until(
                &build_dir,
                &plan,
                options.requirement_boundary,
                options.yes,
            )?;
            journal.record_action(
                "requirements:check",
                &requirements_gate,
                crate::execution::ladder::ExecutionStatus::Succeeded,
                "requirement status checked",
            );
            Some(outcome)
        } else {
            None
        };
        let execution_mode = if options.compile_only {
            crate::model::manifest::ExecutionMode::CompileOnly
        } else {
            crate::model::manifest::ExecutionMode::Normal
        };
        let script_state_root = if options.run_scripts {
            options
                .script_state_root
                .clone()
                .map_or_else(crate::execution::script::materialise_state_root, Ok)?
        } else {
            PathBuf::new()
        };
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
                            run_scripts: options.run_scripts,
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
                                | crate::model::manifest::ScriptOutcomeStatus::ManualSkip
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
                        product_skipped_requirement_gates.clone(),
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
                journal.rungs.iter().find(|record| record.id == rung),
                Some(record) if record.status == crate::execution::ladder::ExecutionStatus::Reused
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
    })()
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

fn materialise_requirement_gates(plan: &crate::model::manifest::BuildPlan) -> Vec<String> {
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
        .collect()
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

/// Evaluate configuration once and persist the executable plan.
///
/// This is the only operation that runs repository Lua. Everything downstream
/// consumes the frozen result.
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
    let _lock = acquire_build_lock(
        lock,
        &lock_path,
        &build_dir,
        crate::storage::locking::Mode::Exclusive,
    )?;
    (|| {
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
    })()
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
