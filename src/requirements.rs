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

pub fn prepare_plan(build_dir: &Path, plan: &BuildPlan, yes: bool) -> Result<BootstrapOutcome> {
    let _environment_lock = EnvironmentLock::exclusive()?;
    reconcile_context(&RequirementContext::build(plan, build_dir), yes, "prepare")
}

/// Reconcile the unified requirement set immediately before its materialise
/// deadline.  This is deliberately separate from product bootstrap: a plan is
/// still the authority until a product has been published.
pub fn prepare_target_plan(
    build_dir: &Path,
    plan: &BuildPlan,
    yes: bool,
) -> Result<BootstrapOutcome> {
    prepare_target_plan_until(
        build_dir,
        plan,
        crate::ladder::CoreRung::MaterialiseAfter,
        yes,
    )
}

pub fn prepare_target_plan_until(
    build_dir: &Path,
    plan: &BuildPlan,
    rung: crate::ladder::CoreRung,
    yes: bool,
) -> Result<BootstrapOutcome> {
    ensure_compatible_platform(&plan.target.platform)?;
    let _environment_lock = EnvironmentLock::exclusive()?;
    let mut eligible = plan.clone();
    eligible
        .requirements
        .retain(|requirement| plan.ladder.before_or_at(&requirement.when, rung));
    reconcile_context(
        &RequirementContext::target_plan(&eligible, build_dir),
        yes,
        "materialise",
    )
}

pub fn authorize_target_plan(
    build_dir: &Path,
    plan: &BuildPlan,
    yes: bool,
) -> Result<RequirementAuthorization> {
    authorize_target_plan_until(
        build_dir,
        plan,
        crate::ladder::CoreRung::MaterialiseAfter,
        yes,
    )
}

pub fn authorize_target_plan_until(
    build_dir: &Path,
    plan: &BuildPlan,
    boundary: crate::ladder::CoreRung,
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
        opened
            .manifest
            .ladder
            .at_or_after(&requirement.when, crate::ladder::CoreRung::DeployBefore)
    });
    authorize_context(
        RequirementContext::target_manifest(&manifest, &opened.product_dir),
        yes,
        "deploy",
    )
}

pub fn prepare_product_deploy_until_authorized(
    build_dir: &Path,
    rung: crate::ladder::CoreRung,
    authorization: &mut RequirementAuthorization,
) -> Result<BootstrapOutcome> {
    prepare_product_deploy_at_authorized(build_dir, &rung.into(), authorization)
}

pub(crate) fn prepare_product_deploy_at_authorized(
    build_dir: &Path,
    rung: &crate::ladder::RungId,
    authorization: &mut RequirementAuthorization,
) -> Result<BootstrapOutcome> {
    let opened = open_build(build_dir)?;
    ensure_compatible_host(&opened.manifest)?;
    let _environment_lock = EnvironmentLock::exclusive()?;
    let mut manifest = opened.manifest.clone();
    manifest.requirements.retain(|requirement| {
        opened
            .manifest
            .ladder
            .at_or_after(&requirement.when, crate::ladder::CoreRung::DeployBefore)
            && opened.manifest.ladder.position(&requirement.when)
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
    rung: crate::ladder::CoreRung,
    authorization: &mut RequirementAuthorization,
) -> Result<BootstrapOutcome> {
    prepare_target_plan_at_authorized(build_dir, plan, &rung.into(), authorization)
}

pub(crate) fn prepare_target_plan_at_authorized(
    build_dir: &Path,
    plan: &BuildPlan,
    rung: &crate::ladder::RungId,
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

pub fn bootstrap(build_dir: &Path, yes: bool) -> Result<BootstrapOutcome> {
    bootstrap_opened(build_dir, yes, None)
}

pub fn bootstrap_exact(
    build_dir: &Path,
    yes: bool,
    expected_build_id: &str,
) -> Result<BootstrapOutcome> {
    bootstrap_opened(build_dir, yes, Some(expected_build_id))
}

fn bootstrap_opened(
    build_dir: &Path,
    yes: bool,
    expected_build_id: Option<&str>,
) -> Result<BootstrapOutcome> {
    let opened = open_build(build_dir)?;
    if let Some(expected_build_id) = expected_build_id
        && opened.manifest.build_id != expected_build_id
    {
        return Err(WombatError::configuration(format!(
            "bootstrap expected build `{expected_build_id}` but opened `{}`; refusing to mutate the host for a different product",
            opened.manifest.build_id
        )));
    }
    let _environment_lock = EnvironmentLock::exclusive()?;
    ensure_compatible_host(&opened.manifest)?;
    let context = RequirementContext::target(&opened);
    reconcile_context(&context, yes, "bootstrap")
}

fn reconcile_context(
    context: &RequirementContext<'_>,
    yes: bool,
    operation_name: &str,
) -> Result<BootstrapOutcome> {
    reconcile_context_inner(context, yes, operation_name, None)
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

fn check_context(context: &RequirementContext<'_>) -> Result<CheckOutcome> {
    let items = context
        .requirements
        .iter()
        .map(|requirement| check_requirement(context, requirement))
        .collect::<Result<Vec<_>>>()?;
    Ok(CheckOutcome {
        build_id: context.id.to_string(),
        items,
    })
}

fn check_requirement(
    context: &RequirementContext<'_>,
    requirement: &Requirement,
) -> Result<CheckItem> {
    let selected = selected_candidate(requirement)?;
    let label = requirement_label(requirement);
    if requirement.kind == RequirementKind::Command {
        let command = selected.name();
        if let Some(path) = which(command) {
            if let Some(minimum) = selected.minimum() {
                let observed = observe_command_version(&path)?;
                if version_at_least(&observed, minimum) {
                    return Ok(CheckItem {
                        requirement: label,
                        provider: requirement.binding.provider.clone(),
                        status: CheckStatus::Satisfied,
                        detail: format!("{} at {}", path.display(), observed),
                    });
                }
                return Ok(CheckItem {
                    requirement: label,
                    provider: requirement.binding.provider.clone(),
                    status: CheckStatus::Outdated,
                    detail: format!("observed {observed}; needs at least {minimum}"),
                });
            }
            return Ok(CheckItem {
                requirement: label,
                provider: requirement.binding.provider.clone(),
                status: CheckStatus::Satisfied,
                detail: path.display().to_string(),
            });
        }
    }
    let provider = provider_for(context.providers, &requirement.binding.provider)?;
    let mut result = match &provider.origin {
        ProviderOrigin::Builtin { .. } => match provider.name.as_str() {
            "brew" => check_brew(&requirement.binding, selected.minimum())?,
            "apt" => check_apt(&requirement.binding, selected.minimum())?,
            name => {
                return Err(WombatError::configuration(format!(
                    "unsupported built-in provider `{name}`"
                )));
            }
        },
        ProviderOrigin::Custom { .. } => check_custom(context, provider, requirement)?,
    };
    result.requirement = label;
    if result.status == CheckStatus::Satisfied {
        for command in &requirement.binding.publications.commands {
            if which(command).is_none() {
                result.status = CheckStatus::Missing;
                result.detail = format!(
                    "package is present but published command `{command}` is absent from PATH"
                );
                break;
            }
        }
    }
    Ok(result)
}

fn check_brew(binding: &ProviderBinding, minimum: Option<&str>) -> Result<CheckItem> {
    let (kind, name) = brew_identity(binding)?;
    let brew = which("brew");
    let Some(brew) = brew else {
        return Ok(provider_item(
            binding,
            CheckStatus::Unavailable,
            "Homebrew is not available on PATH",
        ));
    };
    let output = run_bounded(
        &brew,
        &["info", "--json=v2", brew_flag(kind), name],
        &BTreeMap::new(),
    )?;
    if !output.status.success() {
        return Ok(provider_item(
            binding,
            CheckStatus::Unavailable,
            &format!("brew info failed: {}", output_detail(&output)),
        ));
    }
    let json: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let installed = installed_brew_versions(&json, kind)?;
    let Some(observed) = installed.last() else {
        return Ok(provider_item(
            binding,
            CheckStatus::Missing,
            "not installed",
        ));
    };
    if minimum.is_some_and(|minimum| !version_at_least(observed, minimum)) {
        return Ok(provider_item(
            binding,
            CheckStatus::Outdated,
            &format!("observed {observed}; needs at least {}", minimum.unwrap()),
        ));
    }
    Ok(provider_item(
        binding,
        CheckStatus::Satisfied,
        &format!("{kind} {name} {observed}"),
    ))
}

fn check_apt(binding: &ProviderBinding, minimum: Option<&str>) -> Result<CheckItem> {
    let name = apt_identity(binding)?;
    let Some(dpkg_query) = which("dpkg-query") else {
        return Ok(provider_item(
            binding,
            CheckStatus::Unavailable,
            "dpkg-query is not available on PATH",
        ));
    };
    let output = run_bounded(
        &dpkg_query,
        &["-W", "-f=${Status}\t${Version}", name],
        &BTreeMap::new(),
    )?;
    if output.status.success() {
        let text = String::from_utf8_lossy(&output.stdout);
        let Some((status, observed)) = text.trim().rsplit_once('\t') else {
            return Ok(provider_item(
                binding,
                CheckStatus::Unavailable,
                "dpkg-query returned an unrecognized package record",
            ));
        };
        if status != "install ok installed" {
            return Ok(provider_item(
                binding,
                CheckStatus::Missing,
                &format!("dpkg status is {status}"),
            ));
        }
        if let Some(minimum) = minimum {
            let Some(dpkg) = which("dpkg") else {
                return Ok(provider_item(
                    binding,
                    CheckStatus::Unavailable,
                    "dpkg is unavailable for Debian version comparison",
                ));
            };
            let comparison = run_bounded(
                &dpkg,
                &["--compare-versions", observed, "ge", minimum],
                &BTreeMap::new(),
            )?;
            if !comparison.status.success() {
                return Ok(provider_item(
                    binding,
                    CheckStatus::Outdated,
                    &format!("observed {observed}; needs at least {minimum}"),
                ));
            }
        }
        return Ok(provider_item(
            binding,
            CheckStatus::Satisfied,
            &format!("package {name} {observed}"),
        ));
    }

    let Some(apt_cache) = which("apt-cache") else {
        return Ok(provider_item(
            binding,
            CheckStatus::Unavailable,
            "apt-cache is not available on PATH",
        ));
    };
    let policy = run_bounded(&apt_cache, &["policy", name], &BTreeMap::new())?;
    if !policy.status.success() {
        return Ok(provider_item(
            binding,
            CheckStatus::Unavailable,
            &format!("apt-cache policy failed: {}", output_detail(&policy)),
        ));
    }
    let policy_text = String::from_utf8_lossy(&policy.stdout);
    let candidate = policy_text
        .lines()
        .find_map(|line| line.trim().strip_prefix("Candidate:").map(str::trim));
    match candidate {
        Some(candidate) if candidate != "(none)" => Ok(provider_item(
            binding,
            CheckStatus::Missing,
            &format!("not installed; candidate {candidate}"),
        )),
        _ => Ok(provider_item(
            binding,
            CheckStatus::Unavailable,
            "no Apt candidate is available",
        )),
    }
}

fn check_custom(
    context: &RequirementContext<'_>,
    provider: &Provider,
    requirement: &Requirement,
) -> Result<CheckItem> {
    let runtime = CustomRuntime::load(context, provider, false, false, false)?;
    let check: Function = runtime.definition.get("check").map_err(|error| {
        WombatError::configuration(format!(
            "provider `{}` requires check(): {error}",
            provider.name
        ))
    })?;
    let value: Value = check
        .call((
            runtime.context.clone(),
            frozen_binding(&requirement.binding)?.to_lua(&runtime.lua)?,
        ))
        .map_err(|error| {
            WombatError::configuration(format!(
                "provider `{}` check failed: {error}",
                provider.name
            ))
        })?;
    parse_custom_status(&requirement.binding, FrozenValue::from_lua(value)?)
}

fn prepare_provider(
    context: &RequirementContext<'_>,
    operation: &ProviderPreparation,
    noninteractive: bool,
) -> Result<()> {
    let provider = provider_for(context.providers, &operation.provider)?;
    match &provider.origin {
        ProviderOrigin::Builtin { .. } if provider.name == "apt" => {
            if operation.identity != "update-index" {
                return Err(WombatError::configuration(format!(
                    "Apt does not recognize preparation `{}`",
                    operation.identity
                )));
            }
            let apt_get = require_command("apt-get", "Apt preparation")?;
            run_mutating(
                &apt_get,
                &["update"],
                &apt_environment(),
                operation.elevated,
                noninteractive,
            )
        }
        ProviderOrigin::Builtin { .. } => Err(WombatError::configuration(format!(
            "built-in provider `{}` does not support preparation",
            provider.name
        ))),
        ProviderOrigin::Custom { .. } => {
            let runtime =
                CustomRuntime::load(context, provider, true, operation.elevated, noninteractive)?;
            let prepare: Function = runtime.definition.get("prepare").map_err(|error| {
                WombatError::configuration(format!(
                    "provider `{}` requires prepare(): {error}",
                    provider.name
                ))
            })?;
            prepare
                .call::<Value>((
                    runtime.context,
                    frozen_preparation(operation)?.to_lua(&runtime.lua)?,
                ))
                .map_err(|error| {
                    WombatError::configuration(format!(
                        "provider `{}` prepare failed: {error}",
                        provider.name
                    ))
                })?;
            Ok(())
        }
    }
}

fn preflight(
    context: &RequirementContext<'_>,
    preparations: &[&ProviderPreparation],
    pending: &[(usize, &CheckItem)],
) -> Result<()> {
    for operation in preparations {
        let provider = provider_for(context.providers, &operation.provider)?;
        match &provider.origin {
            ProviderOrigin::Builtin { .. } if provider.name == "apt" => {
                require_command("apt-get", "Apt preparation")?;
                preflight_elevation(operation.elevated)?;
            }
            ProviderOrigin::Builtin { .. } => {
                return Err(WombatError::configuration(format!(
                    "built-in provider `{}` does not support preparation",
                    provider.name
                )));
            }
            ProviderOrigin::Custom { .. } => {
                let runtime = CustomRuntime::load(context, provider, false, false, false)?;
                runtime
                    .definition
                    .get::<Function>("prepare")
                    .map_err(|error| {
                        WombatError::configuration(format!(
                            "provider `{}` requires prepare(): {error}",
                            provider.name
                        ))
                    })?;
                preflight_elevation(operation.elevated)?;
            }
        }
    }
    for (index, _item) in pending {
        let requirement = &context.requirements[*index];
        let provider = provider_for(context.providers, &requirement.binding.provider)?;
        match &provider.origin {
            ProviderOrigin::Builtin { .. } if provider.name == "brew" => {
                let (kind, name) = brew_identity(&requirement.binding)?;
                let brew = which("brew").ok_or_else(|| {
                    WombatError::configuration("cannot bootstrap because Homebrew is not available")
                })?;
                let operation = brew_operation(&requirement.binding)?;
                let output = run_bounded(
                    &brew,
                    &[operation, "--dry-run", brew_flag(kind), name],
                    &brew_environment(),
                )?;
                if !output.status.success() {
                    return Err(WombatError::configuration(format!(
                        "Homebrew preflight failed for `{name}`: {}",
                        output_detail(&output)
                    )));
                }
            }
            ProviderOrigin::Builtin { .. } if provider.name == "apt" => {
                let name = apt_identity(&requirement.binding)?;
                let apt_get = require_command("apt-get", "Apt preflight")?;
                let output = run_bounded(
                    &apt_get,
                    &["--simulate", "install", name],
                    &apt_environment(),
                )?;
                if !output.status.success() {
                    return Err(WombatError::configuration(format!(
                        "Apt preflight failed for `{name}`: {}",
                        output_detail(&output)
                    )));
                }
                preflight_elevation(true)?;
            }
            ProviderOrigin::Builtin { .. } => unreachable!(),
            ProviderOrigin::Custom { .. } => {
                // Loading validates the exact payload and provider surface before confirmation.
                let _ = CustomRuntime::load(context, provider, false, false, false)?;
            }
        }
    }
    Ok(())
}

fn reconcile_requirement(
    context: &RequirementContext<'_>,
    requirement: &Requirement,
    status: CheckStatus,
    noninteractive: bool,
) -> Result<()> {
    let provider = provider_for(context.providers, &requirement.binding.provider)?;
    match &provider.origin {
        ProviderOrigin::Builtin { .. } if provider.name == "brew" => {
            let (kind, name) = brew_identity(&requirement.binding)?;
            let brew = which("brew").ok_or_else(|| {
                WombatError::configuration("Homebrew disappeared before bootstrap")
            })?;
            let operation = brew_operation(&requirement.binding)?;
            let child_status = Command::new(&brew)
                .args([operation, brew_flag(kind), name])
                .envs(brew_environment())
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status()
                .map_err(|error| WombatError::io(&brew, error))?;
            if !child_status.success() {
                return Err(WombatError::configuration(format!(
                    "Homebrew {operation} failed for `{name}` with {child_status}"
                )));
            }
        }
        ProviderOrigin::Builtin { .. } if provider.name == "apt" => {
            let name = apt_identity(&requirement.binding)?;
            let apt_get = require_command("apt-get", "Apt bootstrap")?;
            run_mutating(
                &apt_get,
                &["install", "--yes", name],
                &apt_environment(),
                true,
                noninteractive,
            )?;
        }
        ProviderOrigin::Builtin { .. } => unreachable!(),
        ProviderOrigin::Custom { .. } => {
            let runtime = CustomRuntime::load(context, provider, true, false, noninteractive)?;
            let reconcile: Function = runtime.definition.get("reconcile").map_err(|error| {
                WombatError::configuration(format!(
                    "provider `{}` requires reconcile(): {error}",
                    provider.name
                ))
            })?;
            reconcile
                .call::<Value>((
                    runtime.context,
                    frozen_binding(&requirement.binding)?.to_lua(&runtime.lua)?,
                    FrozenValue::Map(BTreeMap::from([(
                        "status".to_string(),
                        FrozenValue::String(status.as_str().to_string()),
                    )]))
                    .to_lua(&runtime.lua)?,
                ))
                .map_err(|error| {
                    WombatError::configuration(format!(
                        "provider `{}` reconcile failed: {error}",
                        provider.name
                    ))
                })?;
        }
    }
    Ok(())
}

struct CustomRuntime {
    lua: Lua,
    definition: Table,
    context: Table,
}

impl CustomRuntime {
    fn load(
        context: &RequirementContext<'_>,
        provider: &Provider,
        mutable: bool,
        elevation_allowed: bool,
        noninteractive: bool,
    ) -> Result<Self> {
        let lua = Lua::new_with(
            StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8,
            LuaOptions::default(),
        )?;
        for name in ["dofile", "load", "loadfile"] {
            lua.globals().set(name, Value::Nil)?;
        }
        let api = runtime_provider_api(&lua)?;
        install_product_require(&lua, context, provider, api)?;
        let entrypoint = match &provider.origin {
            ProviderOrigin::Custom { entrypoint, .. } => entrypoint,
            ProviderOrigin::Builtin { .. } => unreachable!(),
        };
        let source = fs::read_to_string(context.payload_root.join(entrypoint))
            .map_err(|error| WombatError::io(context.payload_root.join(entrypoint), error))?;
        let definition: Table = lua
            .load(&source)
            .set_name(format!("@providers/{entrypoint}"))
            .eval()
            .map_err(|error| {
                WombatError::configuration(format!(
                    "provider `{}` load failed: {error}",
                    provider.name
                ))
            })?;
        let context = process_context(&lua, mutable, elevation_allowed, noninteractive)?;
        Ok(Self {
            lua,
            definition,
            context,
        })
    }
}

fn runtime_provider_api(lua: &Lua) -> Result<Table> {
    let api = lua.create_table()?;
    api.set("define", lua.create_function(|_, table: Table| Ok(table))?)?;
    api.set(
        "binding",
        lua.create_function(|_, table: Table| {
            table.set("kind", "binding")?;
            Ok(table)
        })?,
    )?;
    api.set(
        "operation",
        lua.create_function(|_, table: Table| {
            table.set("kind", "operation")?;
            Ok(table)
        })?,
    )?;
    api.set(
        "unsupported",
        lua.create_function(|lua, reason: String| {
            let table = lua.create_table()?;
            table.set("kind", "unsupported")?;
            table.set("reason", reason)?;
            Ok(table)
        })?,
    )?;
    for (name, status) in [
        ("satisfied", "satisfied"),
        ("missing", "missing"),
        ("outdated", "outdated"),
        ("unavailable", "unavailable"),
    ] {
        api.set(
            name,
            lua.create_function(move |lua, detail: Value| {
                let table = lua.create_table()?;
                table.set("status", status)?;
                if !detail.is_nil() {
                    table.set("detail", detail)?;
                }
                Ok(table)
            })?,
        )?;
    }
    Ok(api)
}

fn install_product_require(
    lua: &Lua,
    context: &RequirementContext<'_>,
    provider: &Provider,
    api: Table,
) -> Result<()> {
    let files = match &provider.origin {
        ProviderOrigin::Custom { files, .. } => files,
        ProviderOrigin::Builtin { .. } => unreachable!(),
    };
    let allowed = files
        .iter()
        .map(|file| {
            (
                file.payload.clone(),
                context.payload_root.join(&file.payload),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let provider_name = provider.name.clone();
    lua.globals().set(
        "require",
        lua.create_function(move |lua, module: String| {
            if module == "wombat.provider" {
                return Ok(Value::Table(api.clone()));
            }
            if module.is_empty()
                || module.split('.').any(|part| {
                    part.is_empty()
                        || !part
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                })
            {
                return Err(mlua::Error::runtime("invalid provider helper module"));
            }
            let relative = module.replace('.', "/");
            let direct = format!("{provider_name}/{relative}.lua");
            let initial = format!("{provider_name}/{relative}/init.lua");
            let path = allowed
                .get(&direct)
                .or_else(|| allowed.get(&initial))
                .ok_or_else(|| {
                    mlua::Error::runtime(format!(
                        "provider helper `{module}` is not in the exact product"
                    ))
                })?;
            let source = fs::read_to_string(path).map_err(mlua::Error::external)?;
            lua.load(&source)
                .set_name(format!("@providers/{module}"))
                .eval::<Value>()
        })?,
    )?;
    Ok(())
}

fn process_context(
    lua: &Lua,
    mutable: bool,
    elevation_allowed: bool,
    noninteractive: bool,
) -> Result<Table> {
    let context = lua.create_table()?;
    context.set(
        "which",
        lua.create_function(|_, (_self, command): (Value, String)| {
            Ok(which(&command).map(|path| path.to_string_lossy().into_owned()))
        })?,
    )?;
    context.set(
        "observe",
        lua.create_function(|lua, (_self, spec): (Value, Table)| {
            let (program, args, env, elevated) = process_spec(spec)?;
            if elevated {
                return Err(mlua::Error::runtime(
                    "observational provider processes cannot request elevation",
                ));
            }
            let output = run_bounded(
                Path::new(&program),
                &args.iter().map(String::as_str).collect::<Vec<_>>(),
                &env,
            )
            .map_err(mlua::Error::external)?;
            output_table(lua, &output)
        })?,
    )?;
    context.set(
        "json_decode",
        lua.create_function(|lua, (_self, source): (Value, String)| {
            let frozen: FrozenValue =
                serde_json::from_str(&source).map_err(mlua::Error::external)?;
            frozen.to_lua(lua)
        })?,
    )?;
    context.set(
        "version_at_least",
        lua.create_function(|_, (_self, observed, minimum): (Value, String, String)| {
            Ok(version_at_least(&observed, &minimum))
        })?,
    )?;
    if mutable {
        context.set(
            "mutate",
            lua.create_function(move |lua, (_self, spec): (Value, Table)| {
                let (program, args, env, elevated) = process_spec(spec)?;
                if elevated && !elevation_allowed {
                    return Err(mlua::Error::runtime(
                        "provider operation did not declare elevation",
                    ));
                }
                let status = mutating_status(
                    Path::new(&program),
                    &args.iter().map(String::as_str).collect::<Vec<_>>(),
                    &env,
                    elevated,
                    noninteractive,
                )
                .map_err(mlua::Error::external)?;
                let table = lua.create_table()?;
                table.set("success", status.success())?;
                table.set("code", status.code())?;
                Ok(table)
            })?,
        )?;
    }
    Ok(context)
}

fn process_spec(spec: Table) -> mlua::Result<ProcessSpec> {
    let program: String = spec.get("program")?;
    let args = spec
        .get::<Option<Table>>("args")?
        .map(|table| table.sequence_values::<String>().collect())
        .transpose()?
        .unwrap_or_default();
    let env = spec
        .get::<Option<Table>>("env")?
        .map(|table| table.pairs::<String, String>().collect())
        .transpose()?
        .unwrap_or_default();
    let elevated = spec.get::<Option<bool>>("elevated")?.unwrap_or(false);
    Ok((program, args, env, elevated))
}

fn output_table(lua: &Lua, output: &Output) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("success", output.status.success())?;
    table.set("code", output.status.code())?;
    table.set(
        "stdout",
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )?;
    table.set(
        "stderr",
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )?;
    Ok(table)
}

fn parse_custom_status(binding: &ProviderBinding, value: FrozenValue) -> Result<CheckItem> {
    let FrozenValue::Map(mut values) = value else {
        return Err(WombatError::configuration(
            "provider check() must return a status table",
        ));
    };
    let status = match values.remove("status") {
        Some(FrozenValue::String(value)) if value == "satisfied" => CheckStatus::Satisfied,
        Some(FrozenValue::String(value)) if value == "missing" => CheckStatus::Missing,
        Some(FrozenValue::String(value)) if value == "outdated" => CheckStatus::Outdated,
        Some(FrozenValue::String(value)) if value == "unavailable" => CheckStatus::Unavailable,
        _ => {
            return Err(WombatError::configuration(
                "provider check() returned an invalid status",
            ));
        }
    };
    let detail = match values.remove("detail") {
        None => status.as_str().to_string(),
        Some(FrozenValue::String(value)) => value,
        Some(value) => serde_json::to_string(&value)?,
    };
    Ok(provider_item(binding, status, &detail))
}

fn ensure_compatible_host(manifest: &Manifest) -> Result<()> {
    ensure_compatible_platform(&manifest.target.platform)
}

fn ensure_compatible_platform(platform: &crate::context::TargetPlatform) -> Result<()> {
    let host = HostContext::observe()?;
    if !platform.locally_compatible_with(&host.platform) {
        return Err(WombatError::configuration(format!(
            "requirements target {}, but this execution environment is {}; check and bootstrap require an exact local OS and architecture",
            platform.compact(),
            host.platform.compact()
        )));
    }
    Ok(())
}

fn selected_candidate(requirement: &Requirement) -> Result<&RequirementCandidate> {
    requirement
        .candidates
        .get(requirement.selected as usize)
        .ok_or_else(|| {
            WombatError::configuration("requirement selection is outside its candidates")
        })
}

fn requirement_label(requirement: &Requirement) -> String {
    format!(
        "{}:{}",
        match requirement.kind {
            RequirementKind::Command => "command",
            RequirementKind::Package => "package",
        },
        requirement.candidates[requirement.selected as usize].name()
    )
}

fn provider_for<'a>(providers: &'a [Provider], name: &str) -> Result<&'a Provider> {
    providers
        .iter()
        .find(|provider| provider.name == name)
        .ok_or_else(|| WombatError::configuration(format!("selected provider `{name}` is absent")))
}

fn provider_item(binding: &ProviderBinding, status: CheckStatus, detail: &str) -> CheckItem {
    CheckItem {
        requirement: String::new(),
        provider: binding.provider.clone(),
        status,
        detail: detail.to_string(),
    }
}

fn frozen_binding(binding: &ProviderBinding) -> Result<FrozenValue> {
    Ok(serde_json::from_value(serde_json::to_value(binding)?)?)
}

fn frozen_preparation(preparation: &ProviderPreparation) -> Result<FrozenValue> {
    Ok(serde_json::from_value(serde_json::to_value(preparation)?)?)
}

fn apt_identity(binding: &ProviderBinding) -> Result<&str> {
    let FrozenValue::Map(data) = &binding.data else {
        return Err(WombatError::configuration("Apt binding data must be a map"));
    };
    match data.get("name") {
        Some(FrozenValue::String(value)) => Ok(value),
        _ => Err(WombatError::configuration("Apt binding lacks package name")),
    }
}

fn brew_identity(binding: &ProviderBinding) -> Result<(&str, &str)> {
    let FrozenValue::Map(data) = &binding.data else {
        return Err(WombatError::configuration(
            "Homebrew binding data must be a map",
        ));
    };
    let kind = match data.get("kind") {
        Some(FrozenValue::String(value)) => value.as_str(),
        _ => return Err(WombatError::configuration("Homebrew binding lacks kind")),
    };
    let name = match data.get("name") {
        Some(FrozenValue::String(value)) => value.as_str(),
        _ => return Err(WombatError::configuration("Homebrew binding lacks name")),
    };
    Ok((kind, name))
}

fn brew_flag(kind: &str) -> &'static str {
    if kind == "cask" {
        "--cask"
    } else {
        "--formula"
    }
}

fn brew_operation(binding: &ProviderBinding) -> Result<&'static str> {
    match check_brew(binding, None)?.status {
        CheckStatus::Satisfied | CheckStatus::Outdated => Ok("upgrade"),
        CheckStatus::Missing => Ok("install"),
        CheckStatus::Unavailable => Err(WombatError::configuration(
            "Homebrew package state became unavailable during bootstrap",
        )),
    }
}

fn installed_brew_versions(json: &serde_json::Value, kind: &str) -> Result<Vec<String>> {
    let entry = json
        .get(if kind == "cask" { "casks" } else { "formulae" })
        .and_then(serde_json::Value::as_array)
        .and_then(|values| values.first())
        .ok_or_else(|| {
            WombatError::configuration("Homebrew returned no matching package record")
        })?;
    let installed = entry.get("installed");
    let versions = match installed {
        Some(serde_json::Value::Array(values)) => values
            .iter()
            .filter_map(|value| {
                value
                    .get("version")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .collect(),
        Some(serde_json::Value::String(value)) => vec![value.clone()],
        _ => Vec::new(),
    };
    Ok(versions)
}

fn brew_environment() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("HOMEBREW_NO_AUTO_UPDATE".to_string(), "1".to_string()),
        ("HOMEBREW_NO_INSTALL_CLEANUP".to_string(), "1".to_string()),
    ])
}

fn apt_environment() -> BTreeMap<String, String> {
    BTreeMap::from([("DEBIAN_FRONTEND".to_string(), "noninteractive".to_string())])
}

fn require_command(command: &str, purpose: &str) -> Result<PathBuf> {
    which(command).ok_or_else(|| {
        WombatError::configuration(format!(
            "{purpose} requires `{command}` to be available on PATH"
        ))
    })
}

fn effective_uid_is_root() -> Result<bool> {
    let id = which("id").unwrap_or_else(|| PathBuf::from("/usr/bin/id"));
    let output = run_bounded(&id, &["-u"], &BTreeMap::new())?;
    if !output.status.success() {
        return Err(WombatError::configuration(format!(
            "could not determine the effective user: {}",
            output_detail(&output)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim() == "0")
}

fn preflight_elevation(elevated: bool) -> Result<()> {
    if elevated && !effective_uid_is_root()? {
        require_command("sudo", "elevated bootstrap")?;
    }
    Ok(())
}

fn authorize_elevation(noninteractive: bool) -> Result<()> {
    if effective_uid_is_root()? {
        return Ok(());
    }
    let sudo = require_command("sudo", "elevated bootstrap")?;
    let mut command = Command::new(&sudo);
    if noninteractive {
        command.args(["-n", "--", "true"]);
    } else {
        command.arg("-v");
    }
    let status = command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| WombatError::io(&sudo, error))?;
    if !status.success() {
        return Err(WombatError::configuration(if noninteractive {
            "non-interactive bootstrap requires existing passwordless sudo authorization"
        } else {
            "sudo authorization failed"
        }));
    }
    Ok(())
}

fn mutating_status(
    program: &Path,
    args: &[&str],
    environment: &BTreeMap<String, String>,
    elevated: bool,
    noninteractive: bool,
) -> Result<std::process::ExitStatus> {
    let through_sudo = elevated && !effective_uid_is_root()?;
    let mut command = if through_sudo {
        let sudo = require_command("sudo", "elevated provider mutation")?;
        let mut command = Command::new(sudo);
        if noninteractive {
            command.arg("-n");
        }
        command.arg("--");
        if !environment.is_empty() {
            command.arg("env");
            for (name, value) in environment {
                command.arg(format!("{name}={value}"));
            }
        }
        command.arg(program);
        command
    } else {
        Command::new(program)
    };
    command.args(args);
    if !through_sudo {
        command.envs(environment);
    }
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| WombatError::io(program, error))
}

fn run_mutating(
    program: &Path,
    args: &[&str],
    environment: &BTreeMap<String, String>,
    elevated: bool,
    noninteractive: bool,
) -> Result<()> {
    let status = mutating_status(program, args, environment, elevated, noninteractive)?;
    if status.success() {
        Ok(())
    } else {
        Err(WombatError::configuration(format!(
            "provider command `{}` failed with {status}",
            program.display()
        )))
    }
}

fn observe_command_version(path: &Path) -> Result<String> {
    let output = run_bounded(path, &["--version"], &BTreeMap::new())?;
    if !output.status.success() {
        return Err(WombatError::configuration(format!(
            "version probe `{}` failed: {}",
            path.display(),
            output_detail(&output)
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    first_version(&stdout)
        .or_else(|| first_version(&stderr))
        .ok_or_else(|| {
            WombatError::configuration(format!(
                "could not parse a version from `{}`",
                path.display()
            ))
        })
}

fn first_version(text: &str) -> Option<String> {
    text.split_whitespace()
        .map(|part| {
            part.trim_matches(|character: char| {
                !character.is_ascii_alphanumeric() && character != '.'
            })
        })
        .find(|part| {
            part.chars()
                .next()
                .is_some_and(|character| character.is_ascii_digit())
                && part.contains('.')
        })
        .map(str::to_string)
}

fn version_at_least(observed: &str, minimum: &str) -> bool {
    let parts = |value: &str| {
        value
            .split(|character: char| !character.is_ascii_digit())
            .filter(|part| !part.is_empty())
            .take(4)
            .map(|part| part.parse::<u64>().unwrap_or(0))
            .collect::<Vec<_>>()
    };
    let mut observed = parts(observed);
    let mut minimum = parts(minimum);
    let length = observed.len().max(minimum.len());
    observed.resize(length, 0);
    minimum.resize(length, 0);
    observed >= minimum
}

fn which(command: &str) -> Option<PathBuf> {
    if command.contains(std::path::MAIN_SEPARATOR) {
        let path = PathBuf::from(command);
        return is_executable_file(&path).then_some(path);
    }
    env::split_paths(&env::var_os("PATH")?).find_map(|directory| {
        let path = directory.join(command);
        is_executable_file(&path).then_some(path)
    })
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn run_bounded(
    path: &Path,
    args: &[&str],
    environment: &BTreeMap<String, String>,
) -> Result<Output> {
    let output = Command::new(path)
        .args(args)
        .envs(environment)
        .output()
        .map_err(|error| WombatError::io(path, error))?;
    if output.stdout.len() > OUTPUT_LIMIT || output.stderr.len() > OUTPUT_LIMIT {
        return Err(WombatError::configuration(format!(
            "process `{}` exceeded the {} byte observation limit",
            path.display(),
            OUTPUT_LIMIT
        )));
    }
    Ok(output)
}

fn output_detail(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    stderr
        .trim()
        .lines()
        .next()
        .or_else(|| stdout.trim().lines().next())
        .unwrap_or("no diagnostic output")
        .to_string()
}

struct EnvironmentLock {
    file: File,
}

impl EnvironmentLock {
    fn shared() -> Result<Self> {
        Self::acquire(false)
    }

    fn exclusive() -> Result<Self> {
        Self::acquire(true)
    }

    fn acquire(exclusive: bool) -> Result<Self> {
        let root = environment_state_root()?;
        fs::create_dir_all(&root).map_err(|error| WombatError::io(&root, error))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
                .map_err(|error| WombatError::io(&root, error))?;
        }
        let path = root.join("environment.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| WombatError::io(&path, error))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|error| WombatError::io(&path, error))?;
        }
        let result = if exclusive {
            file.try_lock()
        } else {
            file.try_lock_shared()
        };
        result.map_err(|error| match error {
            TryLockError::WouldBlock => WombatError::configuration(
                "another Wombat check or bootstrap owns the local environment lock",
            ),
            TryLockError::Error(error) => WombatError::io(&path, error),
        })?;
        Ok(Self { file })
    }
}

impl Drop for EnvironmentLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn environment_state_root() -> Result<PathBuf> {
    if let Some(root) = env::var_os("XDG_STATE_HOME") {
        let root = PathBuf::from(root);
        if !root.is_absolute() {
            return Err(WombatError::configuration(
                "XDG_STATE_HOME must be absolute",
            ));
        }
        return Ok(root.join("wombat"));
    }
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| WombatError::configuration("HOME is not set"))?;
    Ok(home.join(".local/state/wombat"))
}

#[cfg(test)]
mod tests {
    use super::{first_version, version_at_least};

    #[test]
    fn loose_versions_are_explicitly_monotonic() {
        assert!(version_at_least("ripgrep 15.2.0", "14.0"));
        assert!(!version_at_least("0.10.4", "0.11.0"));
        assert_eq!(
            first_version("tool version 2.3.4\n").as_deref(),
            Some("2.3.4")
        );
    }
}
