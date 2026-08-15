//! The frozen build plan: how it is derived, persisted, and identified.
//!
//! Construction evaluates Lua once and freezes the result here. The plan is the
//! complete set of operations Wombat may perform — nothing can appear later
//! because a condition changed at execution time, which is what makes
//! `plan inspect` a real answer rather than a forecast.
//!
//! `plan_id` digests configuration content only. Where the repository sits on
//! disk and which release built it are deliberately excluded, so the same
//! configuration yields the same plan on any machine. Publication is staged and
//! atomic for the same reason products are: a half-written plan must never be
//! mistaken for a complete one.
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use serde::Serialize;
use tempfile::Builder;

use crate::model::manifest::{
    BUILD_PLAN_FORMAT_VERSION, BuildPlan, CONSTRUCTION_VERSION, EvaluatedManifest,
    EvaluatedProduction, PlannedArtifact, PlannedProduction, Provider, ProviderOrigin,
    RendererIdentity,
};
use crate::{Result, WombatError};

const WOMBAT_VERSION: &str = env!("CARGO_PKG_VERSION");
const TEMPLATE_RENDERER_NAME: &str = "handlebars";
const TEMPLATE_CONTRACT_VERSION: u32 = 1;

pub(crate) fn freeze(source_root: &Path, desired: &EvaluatedManifest) -> Result<BuildPlan> {
    let mut artifacts = Vec::with_capacity(desired.artifacts.len());
    for artifact in &desired.artifacts {
        let production = match &artifact.production {
            EvaluatedProduction::Static => {
                let path = source_root.join(&artifact.source);
                crate::model::source::validate_source_components(source_root, &path)?;
                let bytes = fs::read(&path).map_err(|error| WombatError::io(&path, error))?;
                PlannedProduction::Static {
                    source_digest: digest(&bytes),
                    executable: executable(
                        &fs::metadata(&path).map_err(|error| WombatError::io(&path, error))?,
                    ),
                }
            }
            EvaluatedProduction::Template { context } => {
                let path = source_root.join(&artifact.source);
                crate::model::source::validate_source_components(source_root, &path)?;
                let bytes = fs::read(&path).map_err(|error| WombatError::io(&path, error))?;
                PlannedProduction::Template {
                    renderer: RendererIdentity {
                        name: TEMPLATE_RENDERER_NAME.to_string(),
                        contract_version: TEMPLATE_CONTRACT_VERSION,
                    },
                    source_digest: digest(&bytes),
                    context: context.clone(),
                    executable: executable(
                        &fs::metadata(&path).map_err(|error| WombatError::io(&path, error))?,
                    ),
                }
            }
            EvaluatedProduction::GeneratedLua {
                content,
                executable,
            } => PlannedProduction::GeneratedLua {
                contract_version: 1,
                content_digest: digest(content),
                size: u64::try_from(content.len())
                    .map_err(|_| WombatError::configuration("generated value exceeds u64"))?,
                executable: *executable,
            },
            EvaluatedProduction::Task { .. } => {
                return Err(WombatError::configuration(
                    "task output cannot exist before build plan execution",
                ));
            }
        };
        artifacts.push(PlannedArtifact {
            source: artifact.source.clone(),
            source_origin: artifact.source_origin.clone(),
            source_projection: artifact.source_projection.clone(),
            production,
            target: artifact.target.clone(),
            owner: artifact.owner.clone(),
            declared_at: artifact.declared_at.clone(),
        });
    }
    let mut plan = BuildPlan {
        format_version: BUILD_PLAN_FORMAT_VERSION,
        construction_version: crate::model::manifest::CONSTRUCTION_VERSION,
        wombat_version: WOMBAT_VERSION.to_string(),
        project: desired.project.clone(),
        plan_id: String::new(),
        project_arguments: desired.project_arguments.clone(),
        sources: desired.sources.clone(),
        inputs: desired.inputs.clone(),
        target: desired.target.clone(),
        observations: desired.observations.clone(),
        process_observations: desired.process_observations.clone(),
        modules: desired.modules.clone(),
        dependencies: desired.dependencies.clone(),
        template_helpers: desired.template_helpers.clone(),
        project_identity: desired.project_identity.clone(),
        ladder: desired.ladder.clone(),
        providers: desired.providers.clone(),
        requirements: desired.requirements.clone(),
        preparations: desired.preparations.clone(),
        tasks: desired.tasks.iter().map(|task| task.task.clone()).collect(),
        scripts: desired.scripts.clone(),
        artifact_policy: desired.artifact_policy,
        artifact_notices: desired.artifact_notices.clone(),
        artifact_selections: desired.artifact_selections.clone(),
        artifacts,
    };
    // Computed last, over the finished plan, so the identity covers every
    // decision construction made rather than a partially assembled one.
    plan.plan_id = compute_id(&plan)?;
    Ok(plan)
}

pub(crate) fn publish(
    build_dir: &Path,
    source_root: &Path,
    plan: &BuildPlan,
    execution: &EvaluatedManifest,
) -> Result<()> {
    // Staged in a sibling directory and renamed into place, for the same reason
    // products are: an interrupted write must not leave a plan that looks
    // complete. `plan.previous` keeps the old one until the swap succeeds.
    let internal = build_dir.join(".wombat");
    let staging = Builder::new()
        .prefix("plan-")
        .tempdir_in(&internal)
        .map_err(|error| WombatError::io(&internal, error))?;
    let plan_path = staging.path().join("plan.json");
    write_json(&plan_path, plan)?;
    write_json(&staging.path().join("execution.json"), execution)?;
    materialise_provider_payloads(source_root, staging.path(), "providers", &plan.providers)?;
    crate::execution::script::publish_payloads(
        source_root,
        staging.path(),
        &plan.scripts,
        crate::execution::script::PayloadKind::Plan,
    )?;
    crate::lua::template_helpers::publish_payloads(
        source_root,
        staging.path(),
        &plan.template_helpers,
    )?;
    sync_directory(staging.path())?;

    let destination = internal.join("plan");
    let previous = internal.join("plan.previous");
    if previous
        .try_exists()
        .map_err(|error| WombatError::io(&previous, error))?
    {
        fs::remove_dir_all(&previous).map_err(|error| WombatError::io(&previous, error))?;
    }
    if destination
        .try_exists()
        .map_err(|error| WombatError::io(&destination, error))?
    {
        fs::rename(&destination, &previous)
            .map_err(|error| WombatError::io(&destination, error))?;
    }
    if let Err(error) = fs::rename(staging.path(), &destination) {
        if previous
            .try_exists()
            .map_err(|restore_error| WombatError::io(&previous, restore_error))?
        {
            fs::rename(&previous, &destination)
                .map_err(|restore_error| WombatError::io(&destination, restore_error))?;
            sync_directory(&internal)?;
        }
        return Err(WombatError::io(&destination, error));
    }
    sync_directory(&internal)?;
    if previous
        .try_exists()
        .map_err(|error| WombatError::io(&previous, error))?
    {
        fs::remove_dir_all(&previous).map_err(|error| WombatError::io(&previous, error))?;
    }
    Ok(())
}

pub fn read(build_dir: &Path) -> Result<BuildPlan> {
    let path = build_dir.join(".wombat/plan/plan.json");
    let bytes = fs::read(&path).map_err(|error| WombatError::io(&path, error))?;
    let plan: BuildPlan = serde_json::from_slice(&bytes)?;
    validate(&plan)?;
    crate::execution::script::verify_payloads(
        &build_dir.join(".wombat/plan"),
        &plan.scripts,
        crate::execution::script::PayloadKind::Plan,
    )?;
    crate::lua::template_helpers::verify_payloads(
        &build_dir.join(".wombat/plan"),
        &plan.template_helpers,
    )?;
    Ok(plan)
}

pub(crate) fn read_execution(build_dir: &Path, plan: &BuildPlan) -> Result<EvaluatedManifest> {
    let path = build_dir.join(".wombat/plan/execution.json");
    let bytes = fs::read(&path).map_err(|error| WombatError::io(&path, error))?;
    let execution: EvaluatedManifest = serde_json::from_slice(&bytes)?;
    if execution.plan_id != plan.plan_id {
        return Err(WombatError::configuration(format!(
            "stored plan execution payload belongs to `{}`, not `{}`",
            execution.plan_id, plan.plan_id
        )));
    }
    Ok(execution)
}

pub fn validate(plan: &BuildPlan) -> Result<()> {
    if plan.format_version != BUILD_PLAN_FORMAT_VERSION {
        return Err(WombatError::configuration(format!(
            "unsupported build plan format version {}; expected {BUILD_PLAN_FORMAT_VERSION}",
            plan.format_version
        )));
    }
    if plan.construction_version != CONSTRUCTION_VERSION {
        return Err(WombatError::configuration(format!(
            "build plan was constructed under construction version {} by Wombat {}, but this is construction version {CONSTRUCTION_VERSION}",
            plan.construction_version, plan.wombat_version
        )));
    }
    let expected = compute_id(plan)?;
    if plan.plan_id != expected {
        return Err(WombatError::configuration(format!(
            "build plan identity mismatch: recorded `{}`, computed `{expected}`",
            plan.plan_id
        )));
    }
    if plan.tasks.iter().any(|task| !task.outputs.is_empty()) {
        return Err(WombatError::configuration(
            "build plan tasks must not contain executed outputs",
        ));
    }
    plan.ladder.validate()?;
    validate_sha_identity(&plan.project_identity, "plan project identity")?;
    validate_actions(&plan.ladder, &plan.tasks, &plan.scripts)?;
    crate::lua::template_helpers::validate_catalog(&plan.template_helpers)?;
    for requirement in &plan.requirements {
        if !plan.ladder.contains(&requirement.when) || plan.ladder.is_container(&requirement.when) {
            return Err(WombatError::configuration(format!(
                "plan requirement targets invalid rung `{}`",
                requirement.when
            )));
        }
    }
    let source_paths = plan
        .sources
        .iter()
        .map(|source| source.path.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    for pack in &plan.template_helpers {
        for location in std::iter::once(&pack.declared_at.primary).chain(&pack.declared_at.callers)
        {
            if !source_paths.contains(location.source.as_str()) {
                return Err(WombatError::configuration(format!(
                    "plan template helper `{}` declaration references uncatalogued source `{}`",
                    pack.module, location.source
                )));
            }
        }
        for source in &pack.sources {
            if !source_paths.contains(source.path.as_str()) {
                return Err(WombatError::configuration(format!(
                    "plan template helper `{}` references uncatalogued source `{}`",
                    pack.module, source.path
                )));
            }
        }
        for export in &pack.exports {
            if !source_paths.contains(export.defined_at.source.as_str()) {
                return Err(WombatError::configuration(format!(
                    "plan template helper `{}` definition references uncatalogued source `{}`",
                    export.name, export.defined_at.source
                )));
            }
        }
    }
    crate::build::validate_artifact_metadata(
        plan.artifact_policy,
        &plan.artifact_notices,
        &plan.artifact_selections,
        &source_paths,
    )?;
    Ok(())
}

fn validate_sha_identity(value: &str, label: &str) -> Result<()> {
    if value.len() != 71
        || !value.starts_with("sha256:")
        || !value[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(WombatError::configuration(format!("{label} is invalid")));
    }
    Ok(())
}

pub(crate) fn validate_actions(
    ladder: &crate::execution::ladder::ExecutionLadder,
    tasks: &[crate::model::manifest::Task],
    scripts: &[crate::model::manifest::Script],
) -> Result<()> {
    let mut identities = std::collections::BTreeSet::new();
    for task in tasks {
        if !identities.insert(("task", task.identity.as_str()))
            || !ladder.contains(&task.at)
            || ladder.is_container(&task.at)
        {
            return Err(WombatError::configuration(format!(
                "plan task `{}` has invalid identity or rung",
                task.identity
            )));
        }
    }
    for script in scripts {
        if !identities.insert(("script", script.identity.as_str()))
            || !ladder.contains(&script.at)
            || ladder.is_container(&script.at)
            || script.payloads.is_empty()
        {
            return Err(WombatError::configuration(format!(
                "plan script `{}` has invalid identity, rung, or payload",
                script.identity
            )));
        }
        for payload in &script.payloads {
            crate::model::path::validate_relative_path(&payload.source, "script payload source")?;
            crate::model::path::validate_relative_path(
                &payload.relative,
                "script payload relative path",
            )?;
            validate_sha_identity(&payload.digest, "script payload digest")?;
        }
    }
    Ok(())
}

fn compute_id(plan: &BuildPlan) -> Result<String> {
    #[derive(Serialize)]
    struct Identity<'a> {
        format_version: u32,
        construction_version: u32,
        sources: &'a [crate::model::manifest::SourceFile],
        inputs: &'a [crate::model::manifest::BuildInput],
        target: &'a crate::model::context::ResolvedTarget,
        observations: &'a [crate::model::manifest::Observation],
        process_observations: &'a [crate::model::manifest::ProcessObservation],
        modules: &'a [crate::model::manifest::ManifestModule],
        dependencies: &'a [crate::model::manifest::Dependency],
        template_helpers: &'a [crate::model::manifest::TemplateHelperPack],
        ladder: &'a crate::execution::ladder::ExecutionLadder,
        providers: &'a [Provider],
        requirements: &'a [crate::model::manifest::Requirement],
        preparations: &'a [crate::model::manifest::ProviderPreparation],
        tasks: &'a [crate::model::manifest::Task],
        scripts: &'a [crate::model::manifest::Script],
        artifact_policy: &'a crate::model::manifest::ArtifactPolicy,
        artifact_notices: &'a [crate::model::manifest::ArtifactNotice],
        artifact_selections: &'a [crate::model::manifest::ArtifactSelection],
        artifacts: &'a [PlannedArtifact],
    }
    let identity = Identity {
        format_version: plan.format_version,
        construction_version: plan.construction_version,
        sources: &plan.sources,
        inputs: &plan.inputs,
        target: &plan.target,
        observations: &plan.observations,
        process_observations: &plan.process_observations,
        modules: &plan.modules,
        dependencies: &plan.dependencies,
        template_helpers: &plan.template_helpers,
        ladder: &plan.ladder,
        providers: &plan.providers,
        requirements: &plan.requirements,
        preparations: &plan.preparations,
        tasks: &plan.tasks,
        scripts: &plan.scripts,
        artifact_policy: &plan.artifact_policy,
        artifact_notices: &plan.artifact_notices,
        artifact_selections: &plan.artifact_selections,
        artifacts: &plan.artifacts,
    };
    Ok(digest(&serde_json::to_vec(&identity)?))
}

fn materialise_provider_payloads(
    source_root: &Path,
    plan_root: &Path,
    scope: &str,
    providers: &[Provider],
) -> Result<()> {
    for provider in providers {
        let ProviderOrigin::Custom { files, .. } = &provider.origin else {
            continue;
        };
        for file in files {
            let source = source_root.join(&file.source);
            crate::model::source::validate_source_components(source_root, &source)?;
            let bytes = fs::read(&source).map_err(|error| WombatError::io(&source, error))?;
            if digest(&bytes) != file.digest || u64::try_from(bytes.len()).ok() != Some(file.size) {
                return Err(WombatError::configuration(format!(
                    "provider source `{}` changed while publishing the plan",
                    file.source
                )));
            }
            let destination = plan_root
                .join("payloads/providers")
                .join(scope)
                .join(&file.payload);
            let parent = destination
                .parent()
                .expect("provider plan payload has a parent");
            fs::create_dir_all(parent).map_err(|error| WombatError::io(parent, error))?;
            let mut output = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&destination)
                .map_err(|error| WombatError::io(&destination, error))?;
            output
                .write_all(&bytes)
                .map_err(|error| WombatError::io(&destination, error))?;
            set_private_file(&output, &destination)?;
        }
    }
    Ok(())
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    crate::storage::atomic::write_json_pretty(path, value, true)
}

fn sync_directory(path: &Path) -> Result<()> {
    crate::storage::atomic::sync_directory(path)
}

fn set_private_file(file: &std::fs::File, path: &Path) -> Result<()> {
    crate::storage::permissions::set_private_file(file, path)
}

#[cfg(unix)]
fn executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_metadata: &fs::Metadata) -> bool {
    false
}

fn digest(bytes: &[u8]) -> String {
    crate::storage::digest::sha256(bytes)
}
