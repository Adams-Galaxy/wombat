use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};
use tempfile::NamedTempFile;

use crate::build::{OpenedBuild, open_build};
use crate::context::HostContext;
use crate::execution::ladder::{CoreRung, ExecutionJournal, ExecutionStatus};
use crate::manifest::{Artifact, Production};
use crate::reconcile::{
    ActualArtifact, ReconciliationAction, ReconciliationPlan, inspect_actual, plan_reconciliation,
    target_key,
};
use crate::state::{AppliedArtifact, LockMode, TargetState, TargetStateGuard, resolve_state_root};
use crate::{Result, WombatError};

mod apply;
mod render;

use apply::execute;
use render::{
    render_diff, render_item, require_deployment_platform, validate_target_compatibility,
};

fn digest_string(bytes: impl AsRef<[u8]>) -> String {
    crate::storage::digest::prefixed_hex(bytes)
}

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
    clean: bool,
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
    let requirement_authorization = if options.requirement_authorization.is_some() {
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
    let state_root = resolve_state_root(options.state_root.as_deref())?;
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
        clean: options.clean,
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
    WombatError::conflict(format!(
        "target has unresolved conflicts; use --conflict skip or --conflict overwrite deliberately: {conflicts}"
    ))
}
