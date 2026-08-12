use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use mlua::{Function, Lua, LuaOptions, StdLib, Table, Value};

use crate::build::{OpenedBuild, open_build};
use crate::context::HostContext;
use crate::frozen::FrozenValue;
use crate::manifest::{
    BuildPlan, Manifest, Provider, ProviderBinding, ProviderOrigin, ProviderPreparation,
    Requirement, RequirementCandidate, RequirementKind,
};
use crate::{Result, WombatError};

mod check;
mod environment;
mod providers;

use check::*;
use environment::EnvironmentLock;
use providers::*;

const OUTPUT_LIMIT: usize = 4 * 1024 * 1024;
type ProcessSpec = (String, Vec<String>, BTreeMap<String, String>, bool);

struct RequirementContext<'a> {
    id: &'a str,
    providers: &'a [Provider],
    requirements: &'a [Requirement],
    preparations: &'a [ProviderPreparation],
    payload_root: PathBuf,
}

impl<'a> RequirementContext<'a> {
    fn target(opened: &'a OpenedBuild) -> Self {
        Self::target_manifest(&opened.manifest, &opened.product_dir)
    }

    fn target_manifest(manifest: &'a Manifest, product_dir: &Path) -> Self {
        Self {
            id: &manifest.build_id,
            providers: &manifest.providers,
            requirements: &manifest.requirements,
            preparations: &manifest.preparations,
            payload_root: product_dir.join("providers"),
        }
    }

    fn build(plan: &'a BuildPlan, build_dir: &Path) -> Self {
        Self {
            id: &plan.plan_id,
            providers: &plan.providers,
            requirements: &plan.requirements,
            preparations: &plan.preparations,
            payload_root: build_dir.join(".wombat/plan/payloads/providers/providers"),
        }
    }

    fn target_plan(plan: &'a BuildPlan, build_dir: &Path) -> Self {
        Self {
            id: &plan.plan_id,
            providers: &plan.providers,
            requirements: &plan.requirements,
            preparations: &plan.preparations,
            payload_root: build_dir.join(".wombat/plan/payloads/providers/providers"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckStatus {
    Satisfied,
    Missing,
    Outdated,
    Unavailable,
}

impl CheckStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Satisfied => "satisfied",
            Self::Missing => "missing",
            Self::Outdated => "outdated",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckItem {
    pub requirement: String,
    pub provider: String,
    pub status: CheckStatus,
    pub detail: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckOutcome {
    pub build_id: String,
    pub items: Vec<CheckItem>,
}

impl CheckOutcome {
    pub fn satisfied(&self) -> bool {
        self.items
            .iter()
            .all(|item| item.status == CheckStatus::Satisfied)
    }

    pub fn operational_failure(&self) -> bool {
        self.items
            .iter()
            .any(|item| item.status == CheckStatus::Unavailable)
    }

    pub fn display(&self) -> String {
        let mut output = format!("requirements for {}\n", self.build_id);
        for item in &self.items {
            output.push_str(&format!(
                "  {:<11} {} via {} — {}\n",
                item.status.as_str(),
                item.requirement,
                item.provider,
                item.detail
            ));
        }
        output
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BootstrapOutcome {
    pub build_id: String,
    pub completed: Vec<String>,
    pub already_satisfied: Vec<String>,
}

/// Ephemeral approval for the provider work displayed at workflow preflight.
/// It is intentionally not serializable and must never enter an execution
/// journal or completed product.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequirementAuthorization {
    approved: BTreeSet<String>,
    prepared_providers: BTreeSet<String>,
}

impl BootstrapOutcome {
    pub fn display(&self) -> String {
        format!(
            "bootstrap complete for {} ({} changed, {} already satisfied)\n",
            self.build_id,
            self.completed.len(),
            self.already_satisfied.len()
        )
    }
}

pub fn check(build_dir: &Path) -> Result<CheckOutcome> {
    let opened = open_build(build_dir)?;
    let _environment_lock = EnvironmentLock::shared()?;
    ensure_compatible_host(&opened.manifest)?;
    check_context(&RequirementContext::target(&opened))
}

pub fn check_plan(build_dir: &Path, plan: &BuildPlan) -> Result<CheckOutcome> {
    let _environment_lock = EnvironmentLock::shared()?;
    check_context(&RequirementContext::build(plan, build_dir))
}

pub fn check_target_plan(build_dir: &Path, plan: &BuildPlan) -> Result<CheckOutcome> {
    ensure_compatible_platform(&plan.target.platform)?;
    let _environment_lock = EnvironmentLock::shared()?;
    check_context(&RequirementContext::target_plan(plan, build_dir))
}

pub fn authorize_target_plan(
    build_dir: &Path,
    plan: &BuildPlan,
    yes: bool,
) -> Result<RequirementAuthorization> {
    authorize_target_plan_until(
        build_dir,
        plan,
        crate::execution::ladder::CoreRung::MaterialiseAfter,
        yes,
    )
}

pub fn authorize_target_plan_until(
    build_dir: &Path,
    plan: &BuildPlan,
    boundary: crate::execution::ladder::CoreRung,
    yes: bool,
) -> Result<RequirementAuthorization> {
    ensure_compatible_platform(&plan.target.platform)?;
    let _environment_lock = EnvironmentLock::shared()?;
    let mut eligible = plan.clone();
    eligible
        .requirements
        .retain(|requirement| plan.ladder.before_or_at(&requirement.when, boundary));
    let context = RequirementContext::target_plan(&eligible, build_dir);
    let initial = check_context(&context)?;
    if initial.operational_failure() {
        return Err(WombatError::configuration(initial.display()));
    }
    let pending = initial
        .items
        .iter()
        .enumerate()
        .filter(|(_, item)| item.status != CheckStatus::Satisfied)
        .collect::<Vec<_>>();
    if pending.is_empty() {
        return Ok(RequirementAuthorization {
            approved: BTreeSet::new(),
            prepared_providers: BTreeSet::new(),
        });
    }
    let pending_providers = pending
        .iter()
        .map(|(_, item)| item.provider.as_str())
        .collect::<BTreeSet<_>>();
    let preparations = context
        .preparations
        .iter()
        .filter(|operation| pending_providers.contains(operation.provider.as_str()))
        .collect::<Vec<_>>();
    preflight(&context, &preparations, &pending)?;
    eprintln!("materialise will reconcile:");
    let mut grouped = BTreeMap::<&str, Vec<&CheckItem>>::new();
    for (_, item) in &pending {
        grouped.entry(&item.provider).or_default().push(item);
    }
    for (provider, items) in grouped {
        eprintln!("  {provider}");
        for operation in preparations
            .iter()
            .filter(|operation| operation.provider == provider)
        {
            eprintln!(
                "    prepare {}{}",
                operation.description,
                if operation.elevated {
                    " (elevated)"
                } else {
                    ""
                }
            );
        }
        for item in items {
            eprintln!("    {} ({})", item.requirement, item.status.as_str());
        }
    }
    confirm("materialise", yes)?;
    let requires_elevation = preparations.iter().any(|operation| operation.elevated)
        || pending
            .iter()
            .any(|(index, _)| context.requirements[*index].binding.provider == "apt");
    if requires_elevation {
        authorize_elevation(yes)?;
    }
    Ok(RequirementAuthorization {
        approved: pending
            .into_iter()
            .map(|(_, item)| item.requirement.clone())
            .collect(),
        prepared_providers: BTreeSet::new(),
    })
}

pub fn authorize_product_deploy(build_dir: &Path, yes: bool) -> Result<RequirementAuthorization> {
    let opened = open_build(build_dir)?;
    ensure_compatible_host(&opened.manifest)?;
    let _environment_lock = EnvironmentLock::shared()?;
    let mut manifest = opened.manifest.clone();
    manifest.requirements.retain(|requirement| {
        opened.manifest.ladder.at_or_after(
            &requirement.when,
            crate::execution::ladder::CoreRung::DeployBefore,
        )
    });
    authorize_context(
        RequirementContext::target_manifest(&manifest, &opened.product_dir),
        yes,
        "deploy",
    )
}

pub fn prepare_product_deploy_until_authorized(
    build_dir: &Path,
    rung: crate::execution::ladder::CoreRung,
    authorization: &mut RequirementAuthorization,
) -> Result<BootstrapOutcome> {
    prepare_product_deploy_at_authorized(build_dir, &rung.into(), authorization)
}

pub(crate) fn prepare_product_deploy_at_authorized(
    build_dir: &Path,
    rung: &crate::execution::ladder::RungId,
    authorization: &mut RequirementAuthorization,
) -> Result<BootstrapOutcome> {
    let opened = open_build(build_dir)?;
    ensure_compatible_host(&opened.manifest)?;
    let _environment_lock = EnvironmentLock::exclusive()?;
    let mut manifest = opened.manifest.clone();
    manifest.requirements.retain(|requirement| {
        opened.manifest.ladder.at_or_after(
            &requirement.when,
            crate::execution::ladder::CoreRung::DeployBefore,
        ) && opened.manifest.ladder.position(&requirement.when)
            <= opened.manifest.ladder.position(rung)
    });
    let context = RequirementContext::target_manifest(&manifest, &opened.product_dir);
    let current = check_context(&context)?;
    if current.operational_failure() {
        return Err(WombatError::configuration(current.display()));
    }
    if let Some(item) = current
        .items
        .iter()
        .filter(|item| item.status != CheckStatus::Satisfied)
        .find(|item| !authorization.approved.contains(&item.requirement))
    {
        return Err(WombatError::configuration(format!(
            "{} became pending after deploy preflight; start a new invocation to approve it",
            item.requirement
        )));
    }
    reconcile_context_authorized(&context, authorization, "deploy")
}

fn authorize_context(
    context: RequirementContext<'_>,
    yes: bool,
    operation_name: &str,
) -> Result<RequirementAuthorization> {
    let initial = check_context(&context)?;
    if initial.operational_failure() {
        return Err(WombatError::configuration(initial.display()));
    }
    let pending = initial
        .items
        .iter()
        .enumerate()
        .filter(|(_, item)| item.status != CheckStatus::Satisfied)
        .collect::<Vec<_>>();
    if pending.is_empty() {
        return Ok(RequirementAuthorization {
            approved: BTreeSet::new(),
            prepared_providers: BTreeSet::new(),
        });
    }
    let pending_providers = pending
        .iter()
        .map(|(_, item)| item.provider.as_str())
        .collect::<BTreeSet<_>>();
    let preparations = context
        .preparations
        .iter()
        .filter(|operation| pending_providers.contains(operation.provider.as_str()))
        .collect::<Vec<_>>();
    preflight(&context, &preparations, &pending)?;
    eprintln!("{operation_name} will reconcile:");
    for (_, item) in &pending {
        eprintln!(
            "  {} via {} ({})",
            item.requirement,
            item.provider,
            item.status.as_str()
        );
    }
    confirm(operation_name, yes)?;
    if preparations.iter().any(|operation| operation.elevated)
        || pending
            .iter()
            .any(|(index, _)| context.requirements[*index].binding.provider == "apt")
    {
        authorize_elevation(yes)?;
    }
    Ok(RequirementAuthorization {
        approved: pending
            .into_iter()
            .map(|(_, item)| item.requirement.clone())
            .collect(),
        prepared_providers: BTreeSet::new(),
    })
}

pub fn prepare_target_plan_until_authorized(
    build_dir: &Path,
    plan: &BuildPlan,
    rung: crate::execution::ladder::CoreRung,
    authorization: &mut RequirementAuthorization,
) -> Result<BootstrapOutcome> {
    prepare_target_plan_at_authorized(build_dir, plan, &rung.into(), authorization)
}

pub(crate) fn prepare_target_plan_at_authorized(
    build_dir: &Path,
    plan: &BuildPlan,
    rung: &crate::execution::ladder::RungId,
    authorization: &mut RequirementAuthorization,
) -> Result<BootstrapOutcome> {
    ensure_compatible_platform(&plan.target.platform)?;
    let _environment_lock = EnvironmentLock::exclusive()?;
    let mut eligible = plan.clone();
    eligible.requirements.retain(|requirement| {
        plan.ladder.position(&requirement.when) <= plan.ladder.position(rung)
    });
    let context = RequirementContext::target_plan(&eligible, build_dir);
    let current = check_context(&context)?;
    if current.operational_failure() {
        return Err(WombatError::configuration(current.display()));
    }
    let newly_pending = current
        .items
        .iter()
        .filter(|item| item.status != CheckStatus::Satisfied)
        .find(|item| !authorization.approved.contains(&item.requirement));
    if let Some(item) = newly_pending {
        return Err(WombatError::configuration(format!(
            "{} became pending after materialise preflight; start a new invocation to approve it",
            item.requirement
        )));
    }
    reconcile_context_authorized(&context, authorization, "materialise")
}

fn reconcile_context_authorized(
    context: &RequirementContext<'_>,
    authorization: &mut RequirementAuthorization,
    operation_name: &str,
) -> Result<BootstrapOutcome> {
    reconcile_context_inner(context, true, operation_name, Some(authorization))
}

fn reconcile_context_inner(
    context: &RequirementContext<'_>,
    yes: bool,
    operation_name: &str,
    mut authorization: Option<&mut RequirementAuthorization>,
) -> Result<BootstrapOutcome> {
    let initial = check_context(context)?;
    if initial.operational_failure() {
        return Err(WombatError::configuration(initial.display()));
    }
    let pending = initial
        .items
        .iter()
        .enumerate()
        .filter(|(_, item)| item.status != CheckStatus::Satisfied)
        .collect::<Vec<_>>();
    if pending.is_empty() {
        return Ok(BootstrapOutcome {
            build_id: context.id.to_string(),
            completed: Vec::new(),
            already_satisfied: initial
                .items
                .iter()
                .map(|item| item.requirement.clone())
                .collect(),
        });
    }
    let pending_providers = pending
        .iter()
        .map(|(_, item)| item.provider.as_str())
        .collect::<BTreeSet<_>>();
    let preparations = context
        .preparations
        .iter()
        .filter(|operation| pending_providers.contains(operation.provider.as_str()))
        .filter(|operation| {
            authorization.as_ref().is_none_or(|authorization| {
                !authorization
                    .prepared_providers
                    .contains(&operation.provider)
            })
        })
        .collect::<Vec<_>>();
    preflight(context, &preparations, &pending)?;
    eprintln!("{operation_name} will reconcile:");
    let mut grouped = BTreeMap::<&str, Vec<&CheckItem>>::new();
    for (_, item) in &pending {
        grouped.entry(&item.provider).or_default().push(item);
    }
    for (provider, items) in grouped {
        eprintln!("  {provider}");
        for operation in preparations
            .iter()
            .filter(|operation| operation.provider == provider)
        {
            eprintln!(
                "    prepare {}{}",
                operation.description,
                if operation.elevated {
                    " (elevated)"
                } else {
                    ""
                }
            );
        }
        for item in items {
            eprintln!("    {} ({})", item.requirement, item.status.as_str());
        }
    }
    if authorization.is_none() {
        confirm(operation_name, yes)?;
    }

    let requires_elevation = preparations.iter().any(|operation| operation.elevated)
        || pending
            .iter()
            .any(|(index, _)| context.requirements[*index].binding.provider == "apt");
    if requires_elevation {
        authorize_elevation(yes)?;
    }

    let mut completed = Vec::new();
    for (index, operation) in preparations.iter().enumerate() {
        if let Err(error) = prepare_provider(context, operation, yes) {
            let remaining = preparations[index..]
                .iter()
                .map(|operation| format!("prepare:{}:{}", operation.provider, operation.identity))
                .chain(pending.iter().map(|(_, item)| item.requirement.clone()))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(error.with_note(format!(
                "completed: {}; remaining: {remaining}; no rollback was attempted",
                if completed.is_empty() {
                    "none".to_string()
                } else {
                    completed.join(", ")
                }
            )));
        }
        completed.push(format!(
            "prepare:{}:{}",
            operation.provider, operation.identity
        ));
        if let Some(authorization) = authorization.as_deref_mut() {
            authorization
                .prepared_providers
                .insert(operation.provider.clone());
        }
    }
    for (pending_index, (index, item)) in pending.iter().enumerate() {
        let requirement = &context.requirements[*index];
        if let Err(error) = reconcile_requirement(context, requirement, item.status, yes) {
            let remaining = pending[pending_index..]
                .iter()
                .map(|(_, item)| item.requirement.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(error.with_note(format!(
                "completed: {}; remaining: {remaining}; no rollback was attempted",
                if completed.is_empty() {
                    "none".to_string()
                } else {
                    completed.join(", ")
                }
            )));
        }
        let post = check_requirement(context, requirement)?;
        if post.status != CheckStatus::Satisfied {
            return Err(WombatError::configuration(format!(
                "{operation_name} reconciled `{}` but post-check reported {}: {}; completed: {}",
                item.requirement,
                post.status.as_str(),
                post.detail,
                if completed.is_empty() {
                    "none".to_string()
                } else {
                    completed.join(", ")
                }
            )));
        }
        completed.push(item.requirement.clone());
    }
    Ok(BootstrapOutcome {
        build_id: context.id.to_string(),
        completed,
        already_satisfied: initial
            .items
            .iter()
            .filter(|item| item.status == CheckStatus::Satisfied)
            .map(|item| item.requirement.clone())
            .collect(),
    })
}

fn confirm(operation_name: &str, yes: bool) -> Result<()> {
    if yes {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        return Err(WombatError::configuration(format!(
            "{operation_name} requires --yes when standard input is not a terminal"
        )));
    }
    eprint!("continue? [y/N] ");
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|error| WombatError::io("standard input", error))?;
    if !matches!(answer.trim(), "y" | "Y" | "yes" | "YES") {
        return Err(WombatError::configuration(format!(
            "{operation_name} cancelled"
        )));
    }
    Ok(())
}
