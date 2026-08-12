use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};
use tempfile::NamedTempFile;

use crate::build::{OpenedBuild, open_build};
use crate::context::HostContext;
use crate::ladder::{CoreRung, ExecutionJournal, ExecutionStatus};
use crate::manifest::{Artifact, Production};
use crate::reconcile::{
    ActualArtifact, ReconciliationAction, ReconciliationPlan, inspect_actual, plan_reconciliation,
    target_key,
};
use crate::state::{AppliedArtifact, LockMode, TargetState, TargetStateGuard, resolve_state_root};
use crate::{Result, WombatError};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeploymentOptions {
    pub build_dir: PathBuf,
    pub target_root: PathBuf,
    pub state_root: Option<PathBuf>,
    pub target_root_explicit: bool,
    pub patch: bool,
    pub host: Option<HostContext>,
    pub yes: bool,
    pub reconcile_requirements: bool,
    pub requirement_authorization: Option<crate::requirements::RequirementAuthorization>,
    pub clean: bool,
    pub rerun_scripts: bool,
    pub allow_host_scripts: bool,
}

impl DeploymentOptions {
    pub fn new(build_dir: impl Into<PathBuf>, target_root: impl Into<PathBuf>) -> Self {
        Self {
            build_dir: build_dir.into(),
            target_root: target_root.into(),
            state_root: None,
            target_root_explicit: true,
            patch: false,
            host: None,
            yes: false,
            reconcile_requirements: false,
            requirement_authorization: None,
            clean: false,
            rerun_scripts: false,
            allow_host_scripts: false,
        }
    }

    pub fn with_state_root(mut self, state_root: impl Into<PathBuf>) -> Self {
        self.state_root = Some(state_root.into());
        self
    }

    pub fn with_target_root_explicit(mut self, explicit: bool) -> Self {
        self.target_root_explicit = explicit;
        self
    }

    pub fn with_patch(mut self, patch: bool) -> Self {
        self.patch = patch;
        self
    }

    pub fn with_host(mut self, host: HostContext) -> Self {
        self.host = Some(host);
        self
    }

    pub fn with_yes(mut self, yes: bool) -> Self {
        self.yes = yes;
        self
    }

    pub fn with_provider_reconciliation(mut self, reconcile: bool) -> Self {
        self.reconcile_requirements = reconcile;
        self
    }

    #[doc(hidden)]
    pub fn with_requirement_authorization(
        mut self,
        authorization: Option<crate::requirements::RequirementAuthorization>,
    ) -> Self {
        self.requirement_authorization = authorization;
        self
    }

    pub fn with_clean(mut self, clean: bool) -> Self {
        self.clean = clean;
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConflictPolicy {
    Ask,
    Fail,
    Skip,
    Overwrite,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConflictResolution {
    Skip,
    Overwrite,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DiffOutcome {
    pub plan: ReconciliationPlan,
    pub output: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplyStatus {
    Unchanged,
    Applied,
    AppliedWithSkips,
}

impl fmt::Display for ApplyStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unchanged => "unchanged",
            Self::Applied => "applied",
            Self::AppliedWithSkips => "applied with skips",
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplyOutcome {
    pub status: ApplyStatus,
    pub build_id: String,
    pub created: usize,
    pub updated: usize,
    pub removed: usize,
    pub state_advanced: usize,
    pub skipped: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug)]
pub struct PreparedApply {
    opened: OpenedBuild,
    state_guard: TargetStateGuard,
    previous: TargetState,
    plan: ReconciliationPlan,
    warnings: Vec<String>,
    requirement_authorization: Option<crate::requirements::RequirementAuthorization>,
    state_root: PathBuf,
    target_root: PathBuf,
    rerun_scripts: bool,
    allow_host_scripts: bool,
}

impl PreparedApply {
    pub fn plan(&self) -> &ReconciliationPlan {
        &self.plan
    }

    pub fn build_id(&self) -> &str {
        &self.opened.manifest.build_id
    }

    pub fn rendered_diff(&self) -> Result<String> {
        render_diff(&self.opened, &self.plan, false)
    }

    pub fn rendered_diff_for(&self, target: &str) -> Result<String> {
        let item = self
            .plan
            .items
            .iter()
            .find(|item| item.target == target)
            .ok_or_else(|| {
                WombatError::configuration(format!("prepared deployment has no target `{target}`"))
            })?;
        let mut output = String::new();
        render_item(
            &mut output,
            &self.opened,
            &self.plan.target_root,
            item,
            true,
        )?;
        Ok(output)
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn apply(self, resolutions: &BTreeMap<String, ConflictResolution>) -> Result<ApplyOutcome> {
        execute(self, resolutions)
    }
}

pub fn diff(options: &DeploymentOptions) -> Result<DiffOutcome> {
    require_deployment_platform()?;
    let opened = open_build(&options.build_dir)?;
    let state_root = resolve_state_root(options.state_root.as_deref())?;
    let state_guard = TargetStateGuard::open(&state_root, &options.target_root, LockMode::Shared)?;
    let previous = state_guard.load()?;
    let plan = plan_reconciliation(&options.target_root, &opened.manifest, &previous)?;
    let mut output = render_diff(&opened, &plan, options.patch)?;
    if let Ok(pending) = crate::plan::read(&options.build_dir)
        && pending.plan_id != opened.manifest.plan_id
    {
        output = format!(
            "warning: diff uses product plan {}; newer pending plan {} is not materialised\n{output}",
            opened.manifest.plan_id, pending.plan_id
        );
    }
    Ok(DiffOutcome { plan, output })
}

pub fn prepare_apply(options: &DeploymentOptions) -> Result<PreparedApply> {
    require_deployment_platform()?;
    let opened = open_build(&options.build_dir)?;
    let host = options.host.clone().map_or_else(HostContext::observe, Ok)?;
    let warnings =
        validate_target_compatibility(&opened.manifest, &host, options.target_root_explicit)?;
    let mut requirement_authorization = if options.requirement_authorization.is_some() {
        options.requirement_authorization.clone()
    } else if options.reconcile_requirements
        && opened.manifest.execution_mode == crate::manifest::ExecutionMode::Normal
        && opened.manifest.requirements.iter().any(|requirement| {
            opened
                .manifest
                .ladder
                .at_or_after(&requirement.when, CoreRung::DeployBefore)
        })
    {
        Some(crate::requirements::authorize_product_deploy(
            &options.build_dir,
            options.yes,
        )?)
    } else {
        None
    };
    if let Some(authorization) = &mut requirement_authorization {
        let apply: crate::ladder::RungId = CoreRung::DeployApply.into();
        let start: crate::ladder::RungId = CoreRung::DeployBefore.into();
        let start = opened.manifest.ladder.position(&start).expect("core rung");
        let end = opened.manifest.ladder.position(&apply).expect("core rung");
        for rung in opened
            .manifest
            .ladder
            .leaf_ids()
            .skip(start)
            .take(end - start + 1)
        {
            crate::requirements::prepare_product_deploy_at_authorized(
                &options.build_dir,
                rung,
                authorization,
            )?;
        }
    }
    let state_root = resolve_state_root(options.state_root.as_deref())?;
    let initial_guard =
        TargetStateGuard::open(&state_root, &options.target_root, LockMode::Exclusive)?;
    if options.clean {
        initial_guard.reset_execution_journal()?;
    }
    drop(initial_guard);
    let state_guard =
        TargetStateGuard::open(&state_root, &options.target_root, LockMode::Exclusive)?;
    let previous = state_guard.load()?;
    let plan = plan_reconciliation(&options.target_root, &opened.manifest, &previous)?;
    Ok(PreparedApply {
        opened,
        state_guard,
        previous,
        plan,
        warnings,
        requirement_authorization,
        state_root,
        target_root: options.target_root.clone(),
        rerun_scripts: options.rerun_scripts,
        allow_host_scripts: options.allow_host_scripts,
    })
}

pub fn apply(options: &DeploymentOptions, policy: ConflictPolicy) -> Result<ApplyOutcome> {
    let prepared = prepare_apply(options)?;
    let conflicts = prepared
        .plan
        .conflicts()
        .map(|item| item.target.clone())
        .collect::<Vec<_>>();
    let resolution = match policy {
        ConflictPolicy::Fail => {
            if conflicts.is_empty() {
                None
            } else {
                return Err(conflict_error(&prepared.plan));
            }
        }
        ConflictPolicy::Skip => Some(ConflictResolution::Skip),
        ConflictPolicy::Overwrite => Some(ConflictResolution::Overwrite),
        ConflictPolicy::Ask => {
            return Err(WombatError::configuration(
                "interactive conflict policy must be resolved by the CLI before apply",
            ));
        }
    };
    let resolutions = resolution.map_or_else(BTreeMap::new, |resolution| {
        conflicts
            .into_iter()
            .map(|target| (target, resolution))
            .collect()
    });
    prepared.apply(&resolutions)
}

pub fn conflict_error(plan: &ReconciliationPlan) -> WombatError {
    let conflicts = plan
        .conflicts()
        .map(|item| {
            format!(
                "{}: {}",
                item.target,
                item.reason.as_deref().unwrap_or("target conflict")
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    WombatError::configuration(format!(
        "target has unresolved conflicts; use --conflict skip or --conflict overwrite deliberately: {conflicts}"
    ))
}

fn execute(
    prepared: PreparedApply,
    resolutions: &BTreeMap<String, ConflictResolution>,
) -> Result<ApplyOutcome> {
    let journal_path = prepared.state_guard.execution_journal_path();
    let plan_id = prepared.opened.manifest.plan_id.clone();
    let prepared_ladder = prepared.opened.manifest.ladder.clone();
    let result = execute_inner(prepared, resolutions);
    if let Err(error) = &result {
        let mut journal = crate::ladder::read_at(&journal_path)
            .map(|journal| {
                journal.reopen_for_ladder(&plan_id, CoreRung::DeployAfter, &prepared_ladder)
            })
            .unwrap_or_else(|_| {
                ExecutionJournal::new_for_ladder(plan_id, CoreRung::DeployAfter, &prepared_ladder)
            });
        let failed_rung = if journal.rungs.iter().any(|(rung, status)| {
            rung == &crate::ladder::RungId::from(CoreRung::DeployApply)
                && *status == ExecutionStatus::Succeeded
        }) {
            CoreRung::DeployAfter
        } else {
            CoreRung::DeployApply
        };
        journal.fail(failed_rung, error);
        let _ = crate::ladder::write_at(&journal_path, &journal);
    }
    result
}

fn execute_inner(
    prepared: PreparedApply,
    resolutions: &BTreeMap<String, ConflictResolution>,
) -> Result<ApplyOutcome> {
    let PreparedApply {
        opened,
        state_guard,
        previous,
        plan,
        warnings,
        mut requirement_authorization,
        state_root,
        target_root,
        rerun_scripts,
        allow_host_scripts,
    } = prepared;
    let journal_path = state_guard.execution_journal_path();
    let mut journal = crate::ladder::read_at(&journal_path)
        .map(|journal| {
            journal.reopen_for_ladder(
                &opened.manifest.plan_id,
                CoreRung::DeployAfter,
                &opened.manifest.ladder,
            )
        })
        .unwrap_or_else(|_| {
            ExecutionJournal::new_for_ladder(
                opened.manifest.plan_id.clone(),
                CoreRung::DeployAfter,
                &opened.manifest.ladder,
            )
        });
    journal.build_id = Some(opened.manifest.build_id.clone());
    journal.configure(
        opened.manifest.execution_mode,
        opened.manifest.skipped_requirement_gates.clone(),
    );
    let deploy_before: crate::ladder::RungId = CoreRung::DeployBefore.into();
    let deploy_apply: crate::ladder::RungId = CoreRung::DeployApply.into();
    let start = opened
        .manifest
        .ladder
        .position(&deploy_before)
        .expect("core rung");
    let apply_position = opened
        .manifest
        .ladder
        .position(&deploy_apply)
        .expect("core rung");
    let conflicts = plan
        .conflicts()
        .map(|item| item.target.clone())
        .collect::<BTreeSet<_>>();
    let provided = resolutions.keys().cloned().collect::<BTreeSet<_>>();
    if conflicts != provided {
        return Err(WombatError::configuration(
            "every conflict must have exactly one resolution before apply",
        ));
    }

    let script_state_root = state_guard.scripts_directory();
    for rung in opened
        .manifest
        .ladder
        .leaf_ids()
        .skip(start)
        .take(apply_position - start)
    {
        journal.set_id(rung, ExecutionStatus::Running);
        crate::ladder::write_at(&journal_path, &journal)?;
        crate::scripts::check_runners(
            &opened
                .manifest
                .scripts
                .iter()
                .filter(|script| &script.at == rung)
                .cloned()
                .collect::<Vec<_>>(),
        )?;
        for outcome in crate::scripts::execute_at(
            &opened.manifest.scripts,
            rung,
            &crate::scripts::ScriptExecutionOptions {
                state_root: &script_state_root,
                payload_root: &opened.product_dir,
                payload_kind: crate::scripts::PayloadKind::Product,
                project_identity: &opened.manifest.project_identity,
                plan_id: &opened.manifest.plan_id,
                build_id: Some(&opened.manifest.build_id),
                execution_mode: opened.manifest.execution_mode,
                allow_host_scripts,
                rerun: rerun_scripts,
                target_root: Some(&target_root),
            },
        )? {
            journal.record_action(
                outcome.identity,
                &outcome.rung,
                match outcome.status {
                    crate::manifest::ScriptOutcomeStatus::Ran => ExecutionStatus::Succeeded,
                    _ => ExecutionStatus::Skipped,
                },
                outcome.reason,
            );
        }
        journal.set_id(rung, ExecutionStatus::Succeeded);
        crate::ladder::write_at(&journal_path, &journal)?;
    }
    journal.set(CoreRung::DeployApply, ExecutionStatus::Running);
    crate::ladder::write_at(&journal_path, &journal)?;

    crate::scripts::check_runners(
        &opened
            .manifest
            .scripts
            .iter()
            .filter(|script| script.at == deploy_apply)
            .cloned()
            .collect::<Vec<_>>(),
    )?;
    for outcome in crate::scripts::execute_at(
        &opened.manifest.scripts,
        &deploy_apply,
        &crate::scripts::ScriptExecutionOptions {
            state_root: &script_state_root,
            payload_root: &opened.product_dir,
            payload_kind: crate::scripts::PayloadKind::Product,
            project_identity: &opened.manifest.project_identity,
            plan_id: &opened.manifest.plan_id,
            build_id: Some(&opened.manifest.build_id),
            execution_mode: opened.manifest.execution_mode,
            allow_host_scripts,
            rerun: rerun_scripts,
            target_root: Some(&target_root),
        },
    )? {
        journal.record_action(
            outcome.identity,
            &outcome.rung,
            match outcome.status {
                crate::manifest::ScriptOutcomeStatus::Ran => ExecutionStatus::Succeeded,
                _ => ExecutionStatus::Skipped,
            },
            outcome.reason,
        );
    }

    for item in &plan.items {
        let current = inspect_actual(&plan.target_root, &item.path)?;
        if current != item.actual {
            return Err(WombatError::configuration(format!(
                "target `{}` changed after deployment planning; no files were modified",
                item.target
            )));
        }
    }

    let mut state_by_target = previous
        .artifacts
        .into_iter()
        .map(|artifact| {
            let artifact = artifact.to_artifact();
            (target_key(&artifact), artifact)
        })
        .collect::<BTreeMap<_, _>>();
    let mut created = 0;
    let mut updated = 0;
    let mut removed = 0;
    let mut state_advanced = 0;
    let mut skipped = Vec::new();

    for item in &plan.items {
        let resolution = resolutions.get(&item.target).copied();
        if resolution == Some(ConflictResolution::Skip) {
            skipped.push(item.target.clone());
            continue;
        }
        let current = inspect_actual(&plan.target_root, &item.path)?;
        if current != item.actual {
            return Err(WombatError::configuration(format!(
                "target `{}` changed during deployment",
                item.target
            )));
        }

        let overwrite = resolution == Some(ConflictResolution::Overwrite);
        match item.action {
            ReconciliationAction::Unchanged => {}
            ReconciliationAction::Create => {
                write_desired(
                    &opened,
                    &plan.target_root,
                    item.desired.as_ref().unwrap(),
                    &item.path,
                    false,
                )?;
                created += 1;
            }
            ReconciliationAction::Adopt
            | ReconciliationAction::AdvanceState
            | ReconciliationAction::Forget => {
                state_advanced += 1;
            }
            ReconciliationAction::Update => {
                write_desired(
                    &opened,
                    &plan.target_root,
                    item.desired.as_ref().unwrap(),
                    &item.path,
                    true,
                )?;
                updated += 1;
            }
            ReconciliationAction::Remove => {
                remove_target(&item.path)?;
                removed += 1;
            }
            ReconciliationAction::Conflict if overwrite => {
                if let Some(desired) = &item.desired {
                    write_desired(&opened, &plan.target_root, desired, &item.path, true)?;
                    if matches!(item.actual, ActualArtifact::Absent) {
                        created += 1;
                    } else {
                        updated += 1;
                    }
                } else {
                    remove_target(&item.path)?;
                    removed += 1;
                }
            }
            ReconciliationAction::Conflict => unreachable!("conflicts were resolved"),
        }

        let key_artifact = item.desired.as_ref().or(item.previous.as_ref()).unwrap();
        let key = target_key(key_artifact);
        if let Some(desired) = &item.desired {
            state_by_target.insert(key, desired.clone());
        } else {
            state_by_target.remove(&key);
        }
    }

    let artifacts = state_by_target
        .into_values()
        .map(AppliedArtifact::from_artifact)
        .collect::<Vec<_>>();
    let complete_build_id = skipped.is_empty().then(|| plan.build_id.clone());
    state_guard.write(&TargetState {
        format_version: crate::state::TARGET_STATE_FORMAT_VERSION,
        target_root: plan
            .target_root
            .to_str()
            .expect("canonical target root was validated as UTF-8")
            .to_string(),
        complete_build_id,
        artifacts,
    })?;
    journal.set(CoreRung::DeployApply, ExecutionStatus::Succeeded);
    crate::ladder::write_at(&journal_path, &journal)?;
    drop(state_guard);
    let deploy_after: crate::ladder::RungId = CoreRung::DeployAfter.into();
    let end = opened
        .manifest
        .ladder
        .position(&deploy_after)
        .expect("core rung");
    for rung in opened
        .manifest
        .ladder
        .leaf_ids()
        .skip(apply_position + 1)
        .take(end - apply_position)
    {
        if let Some(authorization) = &mut requirement_authorization {
            crate::requirements::prepare_product_deploy_at_authorized(
                &opened.requested_build_dir,
                rung,
                authorization,
            )?;
        }
        journal.set_id(rung, ExecutionStatus::Running);
        crate::ladder::write_at(&journal_path, &journal)?;
        crate::scripts::check_runners(
            &opened
                .manifest
                .scripts
                .iter()
                .filter(|script| &script.at == rung)
                .cloned()
                .collect::<Vec<_>>(),
        )?;
        for outcome in crate::scripts::execute_at(
            &opened.manifest.scripts,
            rung,
            &crate::scripts::ScriptExecutionOptions {
                state_root: &script_state_root,
                payload_root: &opened.product_dir,
                payload_kind: crate::scripts::PayloadKind::Product,
                project_identity: &opened.manifest.project_identity,
                plan_id: &opened.manifest.plan_id,
                build_id: Some(&opened.manifest.build_id),
                execution_mode: opened.manifest.execution_mode,
                allow_host_scripts,
                rerun: rerun_scripts,
                target_root: Some(&target_root),
            },
        )? {
            journal.record_action(
                outcome.identity,
                &outcome.rung,
                match outcome.status {
                    crate::manifest::ScriptOutcomeStatus::Ran => ExecutionStatus::Succeeded,
                    _ => ExecutionStatus::Skipped,
                },
                outcome.reason,
            );
        }
        journal.set_id(rung, ExecutionStatus::Succeeded);
        crate::ladder::write_at(&journal_path, &journal)?;
    }
    let state_guard = TargetStateGuard::open(&state_root, &target_root, LockMode::Exclusive)?;
    crate::ladder::write_at(&state_guard.execution_journal_path(), &journal)?;

    let changed = created + updated + removed + state_advanced;
    let status = if !skipped.is_empty() {
        ApplyStatus::AppliedWithSkips
    } else if changed == 0 {
        ApplyStatus::Unchanged
    } else {
        ApplyStatus::Applied
    };
    Ok(ApplyOutcome {
        status,
        build_id: plan.build_id,
        created,
        updated,
        removed,
        state_advanced,
        skipped,
        warnings,
    })
}

fn write_desired(
    opened: &OpenedBuild,
    target_root: &Path,
    artifact: &Artifact,
    target: &Path,
    replace: bool,
) -> Result<()> {
    let parent = target.parent().expect("artifact targets have a parent");
    ensure_safe_parents(parent)?;
    let source = product_path(opened, artifact);
    let mut input = File::open(&source).map_err(|error| WombatError::io(&source, error))?;
    let mut temporary =
        NamedTempFile::new_in(parent).map_err(|error| WombatError::io(parent, error))?;
    let metadata_before = input
        .metadata()
        .map_err(|error| WombatError::io(&source, error))?;
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = input
            .read(&mut buffer)
            .map_err(|error| WombatError::io(&source, error))?;
        if count == 0 {
            break;
        }
        temporary
            .write_all(&buffer[..count])
            .map_err(|error| WombatError::io(temporary.path(), error))?;
        hasher.update(&buffer[..count]);
        size = size
            .checked_add(u64::try_from(count).expect("buffer length fits u64"))
            .ok_or_else(|| WombatError::configuration("build artifact exceeds u64"))?;
    }
    let metadata_after = input
        .metadata()
        .map_err(|error| WombatError::io(&source, error))?;
    if metadata_before.len() != metadata_after.len()
        || size != artifact.content.size
        || digest_string(hasher.finalize()) != artifact.content.digest
    {
        return Err(WombatError::configuration(format!(
            "verified build artifact `{}` changed while it was being applied",
            source.display()
        )));
    }
    set_mode(temporary.as_file(), temporary.path(), artifact)?;
    temporary
        .as_file_mut()
        .sync_all()
        .map_err(|error| WombatError::io(temporary.path(), error))?;
    if replace {
        temporary
            .persist(target)
            .map_err(|error| WombatError::io(target, error.error))?;
    } else {
        temporary.persist_noclobber(target).map_err(|error| {
            WombatError::configuration(format!(
                "target `{}` appeared during deployment: {}",
                target.display(),
                error.error
            ))
        })?;
    }
    sync_directory(parent)?;
    let actual = inspect_actual(target_root, target)?;
    if !crate::reconcile::actual_matches(&actual, artifact) {
        return Err(WombatError::configuration(format!(
            "deployed target `{}` did not verify",
            target.display()
        )));
    }
    Ok(())
}

fn remove_target(target: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(target).map_err(|error| WombatError::io(target, error))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(WombatError::configuration(format!(
            "refusing to remove non-regular target `{}`",
            target.display()
        )));
    }
    fs::remove_file(target).map_err(|error| WombatError::io(target, error))?;
    sync_directory(target.parent().expect("target files have parents"))
}

fn ensure_safe_parents(parent: &Path) -> Result<()> {
    let mut missing = Vec::new();
    let mut current = parent;
    loop {
        match fs::symlink_metadata(current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
                    return Err(WombatError::configuration(format!(
                        "target parent `{}` must be a non-symlink directory",
                        current.display()
                    )));
                }
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(current.to_path_buf());
                current = current.parent().ok_or_else(|| {
                    WombatError::configuration("cannot create target parent above filesystem root")
                })?;
            }
            Err(error) => return Err(WombatError::io(current, error)),
        }
    }
    for directory in missing.iter().rev() {
        fs::create_dir(directory).map_err(|error| WombatError::io(directory, error))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(directory, fs::Permissions::from_mode(0o755))
                .map_err(|error| WombatError::io(directory, error))?;
        }
        sync_directory(
            directory
                .parent()
                .expect("created directories have parents"),
        )?;
    }
    Ok(())
}

fn product_path(opened: &OpenedBuild, artifact: &Artifact) -> PathBuf {
    opened.product_dir.join("tree").join(&artifact.target.path)
}

fn set_mode(file: &File, path: &Path, artifact: &Artifact) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = crate::reconcile::expected_mode(artifact);
        file.set_permissions(fs::Permissions::from_mode(mode))
            .map_err(|error| WombatError::io(path, error))?;
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    let directory = File::open(path).map_err(|error| WombatError::io(path, error))?;
    directory
        .sync_all()
        .map_err(|error| WombatError::io(path, error))
}

fn render_diff(
    opened: &OpenedBuild,
    plan: &ReconciliationPlan,
    all_patches: bool,
) -> Result<String> {
    let mut output = String::new();
    let mut counts = BTreeMap::<&'static str, usize>::new();
    for item in &plan.items {
        if item.action == ReconciliationAction::Unchanged {
            continue;
        }
        *counts.entry(action_word(item.action)).or_default() += 1;
        let include_patch = all_patches
            || matches!(
                item.action,
                ReconciliationAction::Update | ReconciliationAction::Conflict
            );
        render_item(&mut output, opened, &plan.target_root, item, include_patch)?;
    }
    if output.is_empty() {
        output.push_str("No differences.\n");
    } else {
        use std::fmt::Write as _;
        let total = counts.values().sum::<usize>();
        let summary = counts
            .into_iter()
            .map(|(action, count)| format!("{count} {action}"))
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(&mut output, "{total} changes: {summary}")
            .expect("writing to a string cannot fail");
    }
    Ok(output)
}

fn render_item(
    output: &mut String,
    opened: &OpenedBuild,
    target_root: &Path,
    item: &crate::reconcile::ReconciliationItem,
    include_patch: bool,
) -> Result<()> {
    use std::fmt::Write as _;
    writeln!(output, "{:?} {}", item.action, item.target).expect("writing to a string cannot fail");
    if let Some(artifact) = item.desired.as_ref().or(item.previous.as_ref()) {
        let producer = match artifact.production {
            Production::Static => "static",
            Production::Template { .. } => "template",
            Production::GeneratedLua { .. } => "generated Lua",
            Production::Task { .. } => "task",
        };
        writeln!(
            output,
            "  owner: {}\n  source: {}\n  production: {producer}",
            artifact.owner, artifact.source
        )
        .expect("writing to a string cannot fail");
    }
    if let Some(reason) = &item.reason {
        writeln!(output, "  conflict: {reason}").expect("writing to a string cannot fail");
    }
    if include_patch {
        append_content_diff(output, opened, target_root, item)?;
    }
    Ok(())
}

const fn action_word(action: ReconciliationAction) -> &'static str {
    match action {
        ReconciliationAction::Unchanged => "unchanged",
        ReconciliationAction::Create => "create",
        ReconciliationAction::Adopt => "adopt",
        ReconciliationAction::AdvanceState => "state-only",
        ReconciliationAction::Update => "update",
        ReconciliationAction::Remove => "remove",
        ReconciliationAction::Forget => "forget",
        ReconciliationAction::Conflict => "conflict",
    }
}

fn validate_target_compatibility(
    manifest: &crate::manifest::Manifest,
    host: &HostContext,
    target_root_explicit: bool,
) -> Result<Vec<String>> {
    let target_os = manifest.target.platform.os.name;
    let host_os = host.platform.os.name;
    if !target_root_explicit && target_os != host_os {
        return Err(WombatError::configuration(format!(
            "build target OS `{}` ({:?}) does not match host OS `{}`; refusing implicit live-root deployment before mutation; pass --target-root deliberately for an alternate root",
            target_os.as_str(),
            manifest.target.origin,
            host_os.as_str()
        )));
    }
    let mut warnings = Vec::new();
    if manifest.target.platform.arch != host.platform.arch {
        warnings.push(format!(
            "build target architecture `{}` differs from host architecture `{}`",
            manifest.target.platform.arch.as_str(),
            host.platform.arch.as_str()
        ));
    }
    Ok(warnings)
}

fn append_content_diff(
    output: &mut String,
    opened: &OpenedBuild,
    target_root: &Path,
    item: &crate::reconcile::ReconciliationItem,
) -> Result<()> {
    let old = if matches!(item.actual, ActualArtifact::File { .. }) {
        let bytes = fs::read(&item.path).map_err(|error| WombatError::io(&item.path, error))?;
        let after = inspect_actual(target_root, &item.path)?;
        if after != item.actual {
            return Err(WombatError::configuration(format!(
                "target `{}` changed while its diff was rendered",
                item.target
            )));
        }
        if let ActualArtifact::File { content, .. } = &item.actual
            && (u64::try_from(bytes.len()).ok() != Some(content.size)
                || digest_string(Sha256::digest(&bytes)) != content.digest)
        {
            return Err(WombatError::configuration(format!(
                "target `{}` changed while its diff was rendered",
                item.target
            )));
        }
        Some(bytes)
    } else {
        None
    };
    let new = item
        .desired
        .as_ref()
        .map(|artifact| {
            let path = product_path(opened, artifact);
            fs::read(&path).map_err(|error| WombatError::io(path, error))
        })
        .transpose()?;
    let old_bytes = old.as_deref().unwrap_or_default();
    let new_bytes = new.as_deref().unwrap_or_default();
    let text = !old_bytes.contains(&0)
        && !new_bytes.contains(&0)
        && std::str::from_utf8(old_bytes).is_ok()
        && std::str::from_utf8(new_bytes).is_ok();
    if text {
        let old_text = std::str::from_utf8(old_bytes).expect("text was validated");
        let new_text = std::str::from_utf8(new_bytes).expect("text was validated");
        let diff = similar::TextDiff::from_lines(old_text, new_text);
        let unified = diff
            .unified_diff()
            .context_radius(3)
            .header(&format!("a/{}", item.target), &format!("b/{}", item.target))
            .to_string();
        if !unified.is_empty() {
            output.push_str(&unified);
            if !unified.ends_with('\n') {
                output.push('\n');
            }
        }
    } else {
        use std::fmt::Write as _;
        let (old_digest, old_size, old_mode) = match &item.actual {
            ActualArtifact::File { content, mode } => {
                (content.digest.as_str(), content.size, format!("{mode:04o}"))
            }
            _ => ("absent", 0, "----".to_string()),
        };
        let (new_digest, new_size, new_mode) = item.desired.as_ref().map_or_else(
            || ("absent", 0, "----".to_string()),
            |artifact| {
                (
                    artifact.content.digest.as_str(),
                    artifact.content.size,
                    format!("{:04o}", crate::reconcile::expected_mode(artifact)),
                )
            },
        );
        writeln!(
            output,
            "  binary: {old_digest} {old_size} bytes mode {old_mode} -> {new_digest} {new_size} bytes mode {new_mode}"
        )
        .expect("writing to a string cannot fail");
    }
    Ok(())
}

fn digest_string(bytes: impl AsRef<[u8]>) -> String {
    let mut output = String::with_capacity(7 + bytes.as_ref().len() * 2);
    output.push_str("sha256:");
    for byte in bytes.as_ref() {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to a string cannot fail");
    }
    output
}

fn require_deployment_platform() -> Result<()> {
    if deployment_platform_supported(std::env::consts::OS) {
        Ok(())
    } else {
        Err(WombatError::configuration(
            "target deployment is currently supported only on macOS and Linux",
        ))
    }
}

fn deployment_platform_supported(os: &str) -> bool {
    matches!(os, "macos" | "linux")
}

#[cfg(test)]
mod tests {
    use super::deployment_platform_supported;

    #[test]
    fn deployment_platform_gate_accepts_only_macos_and_linux() {
        assert!(deployment_platform_supported("macos"));
        assert!(deployment_platform_supported("linux"));
        assert!(!deployment_platform_supported("windows"));
        assert!(!deployment_platform_supported("freebsd"));
    }
}
