use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;

use serde::Serialize;
use sha2::{Digest, Sha256};
use tempfile::{Builder, NamedTempFile};

use crate::manifest::{
    BUILD_PLAN_FORMAT_VERSION, BuildPlan, EvaluatedManifest, EvaluatedProduction, PlannedArtifact,
    PlannedProduction, Provider, ProviderOrigin, RendererIdentity,
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
                crate::source::validate_source_components(source_root, &path)?;
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
                crate::source::validate_source_components(source_root, &path)?;
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
        wombat_version: WOMBAT_VERSION.to_string(),
        plan_id: String::new(),
        sources: desired.sources.clone(),
        inputs: desired.inputs.clone(),
        target: desired.target.clone(),
        observations: desired.observations.clone(),
        modules: desired.modules.clone(),
        dependencies: desired.dependencies.clone(),
        build_providers: desired.build_providers.clone(),
        build_requirements: desired.build_requirements.clone(),
        build_preparations: desired.build_preparations.clone(),
        providers: desired.providers.clone(),
        requirements: desired.requirements.clone(),
        preparations: desired.preparations.clone(),
        tasks: desired.tasks.iter().map(|task| task.task.clone()).collect(),
        artifact_policy: desired.artifact_policy,
        artifact_notices: desired.artifact_notices.clone(),
        artifact_selections: desired.artifact_selections.clone(),
        artifacts,
    };
    plan.plan_id = compute_id(&plan)?;
    Ok(plan)
}

pub(crate) fn publish(build_dir: &Path, source_root: &Path, plan: &BuildPlan) -> Result<()> {
    let internal = build_dir.join(".wombat");
    let staging = Builder::new()
        .prefix("plan-")
        .tempdir_in(&internal)
        .map_err(|error| WombatError::io(&internal, error))?;
    let plan_path = staging.path().join("plan.json");
    write_json(&plan_path, plan)?;
    materialise_provider_payloads(source_root, staging.path(), "build", &plan.build_providers)?;
    materialise_provider_payloads(source_root, staging.path(), "target", &plan.providers)?;
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
    Ok(plan)
}

pub fn validate(plan: &BuildPlan) -> Result<()> {
    if plan.format_version != BUILD_PLAN_FORMAT_VERSION {
        return Err(WombatError::configuration(format!(
            "unsupported build plan format version {}; expected {BUILD_PLAN_FORMAT_VERSION}",
            plan.format_version
        )));
    }
    if plan.wombat_version != WOMBAT_VERSION {
        return Err(WombatError::configuration(format!(
            "build plan was produced by Wombat {}, but this is Wombat {WOMBAT_VERSION}",
            plan.wombat_version
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
    let source_paths = plan
        .sources
        .iter()
        .map(|source| source.path.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    crate::build::validate_artifact_metadata(
        plan.artifact_policy,
        &plan.artifact_notices,
        &plan.artifact_selections,
        &source_paths,
    )?;
    Ok(())
}

fn compute_id(plan: &BuildPlan) -> Result<String> {
    #[derive(Serialize)]
    struct Identity<'a> {
        format_version: u32,
        wombat_version: &'a str,
        sources: &'a [crate::manifest::SourceFile],
        inputs: &'a [crate::manifest::BuildInput],
        target: &'a crate::context::ResolvedTarget,
        observations: &'a [crate::manifest::Observation],
        modules: &'a [crate::manifest::ManifestModule],
        dependencies: &'a [crate::manifest::Dependency],
        build_providers: &'a [Provider],
        build_requirements: &'a [crate::manifest::Requirement],
        build_preparations: &'a [crate::manifest::ProviderPreparation],
        providers: &'a [Provider],
        requirements: &'a [crate::manifest::Requirement],
        preparations: &'a [crate::manifest::ProviderPreparation],
        tasks: &'a [crate::manifest::Task],
        artifact_policy: &'a crate::manifest::ArtifactPolicy,
        artifact_notices: &'a [crate::manifest::ArtifactNotice],
        artifact_selections: &'a [crate::manifest::ArtifactSelection],
        artifacts: &'a [PlannedArtifact],
    }
    let identity = Identity {
        format_version: plan.format_version,
        wombat_version: &plan.wombat_version,
        sources: &plan.sources,
        inputs: &plan.inputs,
        target: &plan.target,
        observations: &plan.observations,
        modules: &plan.modules,
        dependencies: &plan.dependencies,
        build_providers: &plan.build_providers,
        build_requirements: &plan.build_requirements,
        build_preparations: &plan.build_preparations,
        providers: &plan.providers,
        requirements: &plan.requirements,
        preparations: &plan.preparations,
        tasks: &plan.tasks,
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
            crate::source::validate_source_components(source_root, &source)?;
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
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    let parent = path.parent().expect("plan files have parents");
    let mut temporary =
        NamedTempFile::new_in(parent).map_err(|error| WombatError::io(parent, error))?;
    set_private_file(temporary.as_file(), temporary.path())?;
    temporary
        .write_all(&bytes)
        .map_err(|error| WombatError::io(temporary.path(), error))?;
    temporary
        .as_file_mut()
        .sync_all()
        .map_err(|error| WombatError::io(temporary.path(), error))?;
    temporary
        .persist(path)
        .map_err(|error| WombatError::io(path, error.error))?;
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| WombatError::io(path, error))
}

fn set_private_file(file: &File, path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| WombatError::io(path, error))?;
    }
    Ok(())
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
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(7 + digest.len() * 2);
    output.push_str("sha256:");
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to a string cannot fail");
    }
    output
}
