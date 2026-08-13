//! Human-readable views of sealed products and stored plans.
//!
//! Inspection opens and verifies a product; it never evaluates repository Lua.
//! That is what lets it explain a product built last month, or on another
//! machine, without the repository being in the state that produced it.
//!
//! These are deliberately human views rather than a second machine-readable
//! schema — `manifest.json` is the product contract, and duplicating it here
//! would create two things to keep in step.
//!
//! Where a view needs execution results, it combines the immutable manifest with
//! the relevant journal. A product with no journal reports that execution state
//! is unavailable rather than inventing an outcome.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::build::open_build;
use crate::model::manifest::{Artifact, BuildPlan, Manifest, Production};
use crate::{Result, WombatError};

mod compare;

use compare::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InspectSection {
    Overview,
    Inputs,
    Target,
    Modules,
    Dependencies,
    Providers,
    Requirements,
    Ladder,
    Scripts,
    Tasks,
    Artifacts,
    Sources,
    Observations,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlanInspectSection {
    Overview,
    Providers,
    Requirements,
    Ladder,
    Scripts,
    Tasks,
    Artifacts,
    Sources,
    Observations,
}

pub fn inspect(build_dir: &Path, section: InspectSection) -> Result<String> {
    let product = open_build(build_dir)?;
    let journal = crate::execution::ladder::read(build_dir)
        .ok()
        .filter(|journal| {
            journal.plan_id == product.manifest.plan_id
                && journal.build_id.as_deref() == Some(product.manifest.build_id.as_str())
        });
    Ok(render_section(&product.manifest, journal.as_ref(), section))
}

pub fn inspect_plan(plan: &BuildPlan, section: PlanInspectSection) -> String {
    match section {
        PlanInspectSection::Overview => format!(
            "Build plan {}\n  format: v{}\n  wombat: {}\n  target: {}/{}\n  sources: {}\n  modules: {}\n  providers: {}\n  requirements: {}\n  tasks: {}\n  artifact selections: {}\n  declared artifacts: {}\n",
            plan.plan_id,
            plan.format_version,
            plan.wombat_version,
            plan.target.platform.os.name.as_str(),
            plan.target.platform.arch.as_str(),
            plan.sources.len(),
            plan.modules.len(),
            plan.providers.len(),
            plan.requirements.len(),
            plan.tasks.len(),
            plan.artifact_selections.len(),
            plan.artifacts.len(),
        ),
        PlanInspectSection::Providers => {
            let mut output = render_list("Providers", plan.providers.iter().map(render_provider));
            output.push_str(&render_list(
                "Preparations",
                plan.preparations.iter().map(render_preparation),
            ));
            output
        }
        PlanInspectSection::Requirements => {
            render_list("Requirements", plan.requirements.iter().map(render_requirement))
        }
        PlanInspectSection::Ladder => render_ladder(&plan.ladder),
        PlanInspectSection::Scripts => render_scripts(&plan.scripts),
        PlanInspectSection::Tasks => render_list(
            "Tasks",
            plan.tasks.iter().map(|task| {
                format!(
                    "{}\n  owner: {}\n  entrypoint: {}\n  digest: {}\n  runner: {:?}\n  cache: {}{}\n  target root: {}\n  declared at: {}",
                    task.identity,
                    task.owner,
                    task.entrypoint,
                    task.entrypoint_digest,
                    task.runner.family_name(),
                    task.cache.enabled,
                    task.cache
                        .revision
                        .as_ref()
                        .map(|revision| format!(" revision={revision}"))
                        .unwrap_or_default(),
                    task.target_root
                        .as_ref()
                        .map(|root| root.path.clone())
                        .unwrap_or_else(|| "none (outputless only)".to_string()),
                    task.declared_at
                )
            }),
        ),
        PlanInspectSection::Artifacts => {
            render_list(
                "Artifact selections",
                plan.artifact_selections.iter().map(render_selection),
            ) + &render_list(
                "Declared artifacts",
                plan.artifacts.iter().map(|artifact| {
                    format!(
                        "{}\n  owner: {}\n  source: {}\n  production: {}\n  declared at: {}",
                        artifact.target.path,
                        artifact.owner,
                        artifact.source,
                        json(&artifact.production),
                        artifact.declared_at
                    )
                }),
            )
        }
        PlanInspectSection::Sources => render_list(
            "Sources",
            plan.sources
                .iter()
                .map(|source| format!("{}\n  digest: {}", source.path, source.digest)),
        ),
        PlanInspectSection::Observations => render_observations(&plan.observations, &plan.process_observations),
    }
}

fn render_observations(
    context: &[crate::model::manifest::Observation],
    processes: &[crate::model::manifest::ProcessObservation],
) -> String {
    let mut output = render_list(
        "Context observations",
        context.iter().map(|observation| {
            format!(
                "{:?}:{}\n  value: {}",
                observation.subject,
                observation.path,
                json(&observation.value)
            )
        }),
    );
    output.push_str(&render_list("Process observations", processes.iter().map(|observation| {
        let invocation = if observation.sensitive { "<redacted>".to_string() } else { json(&observation.invocation) };
        format!("{}\n  cwd: {}\n  status: {}{}{}\n  stdout: {} {}\n  stderr: {} {}\n  declared at: {}",
            invocation, observation.cwd, if observation.ok { "success" } else { "failure" },
            observation.code.map(|code| format!(" code={code}")).unwrap_or_default(),
            observation.signal.map(|signal| format!(" signal={signal}")).unwrap_or_default(),
            observation.stdout_size, observation.stdout_digest, observation.stderr_size, observation.stderr_digest, observation.declared_at)
    })));
    output
}

fn render_ladder(ladder: &crate::execution::ladder::ExecutionLadder) -> String {
    render_list(
        &format!("Ladder {}", ladder.name),
        ladder.flattened.iter().map(|rung| {
            format!(
                "{}{}\n  kind: {}\n  parent: {}",
                "  ".repeat(usize::from(rung.depth)),
                rung.id,
                rung.core
                    .map_or_else(|| "custom".to_string(), |core| format!("core {:?}", core)),
                rung.parent
                    .as_ref()
                    .map_or_else(|| "top-level".to_string(), ToString::to_string)
            )
        }),
    )
}

fn render_scripts(scripts: &[crate::model::manifest::Script]) -> String {
    render_list(
        "Scripts",
        scripts.iter().map(|script| {
            format!(
                "{}\n  owner: {}\n  entrypoint: {}\n  rung: {}\n  schedule: {:?}\n  scope: {:?}\n  runner: {:?}\n  payloads: {}\n  declared at: {}",
                script.identity,
                script.owner,
                script.entrypoint,
                script.at,
                script.schedule,
                script.scope,
                script.runner.family_name(),
                script.payloads.len(),
                script.declared_at
            )
        }),
    )
}

fn render_provider(provider: &crate::model::manifest::Provider) -> String {
    format!(
        "{} (priority {})\n  origin: {}\n  config: {}\n  declared at: {}",
        provider.name,
        provider.priority,
        match &provider.origin {
            crate::model::manifest::ProviderOrigin::Builtin { contract_version } => {
                format!("built-in contract v{contract_version}")
            }
            crate::model::manifest::ProviderOrigin::Custom { entrypoint, files } => {
                format!("custom {entrypoint}, {} files", files.len())
            }
        },
        json(&provider.config),
        provider.declared_at
    )
}

fn render_preparation(operation: &crate::model::manifest::ProviderPreparation) -> String {
    format!(
        "{}:{}\n  description: {}\n  elevated: {}\n  data: {}",
        operation.provider,
        operation.identity,
        operation.description,
        operation.elevated,
        json(&operation.data)
    )
}

pub fn explain(
    build_dir: &Path,
    selector: &str,
    source_root: Option<&Path>,
    current_home: Option<&Path>,
) -> Result<String> {
    let product = open_build(build_dir)?;
    if let Some(identity) = selector.strip_prefix("task:") {
        let task = product
            .manifest
            .tasks
            .iter()
            .find(|task| task.identity == identity)
            .ok_or_else(|| {
                WombatError::configuration(format!(
                    "no task in build `{}` matches `{selector}`",
                    product.manifest.build_id
                ))
            })?;
        return Ok(format!(
            "Task {}\n  build: {}\n  plan: {}\n  owner: {}\n  entrypoint: {}\n  entrypoint digest: {}\n  runner: {}\n  params: {}\n  cache: {}{}\n  logs: {:?}\n  outputs: {}\n  declared at: {}\n",
            task.identity,
            product.manifest.build_id,
            product.manifest.plan_id,
            task.owner,
            task.entrypoint,
            task.entrypoint_digest,
            json(&task.runner),
            json(&task.params),
            task.cache.enabled,
            task.cache
                .revision
                .as_ref()
                .map(|revision| format!(" revision={revision}"))
                .unwrap_or_default(),
            task.logs,
            task.outputs.len(),
            task.declared_at
        ));
    }
    if let Some(name) = selector.strip_prefix("provider:") {
        let provider = product
            .manifest
            .providers
            .iter()
            .find(|provider| provider.name == name)
            .ok_or_else(|| {
                WombatError::configuration(format!(
                    "no provider in build `{}` matches `{selector}`",
                    product.manifest.build_id
                ))
            })?;
        let mut output = format!(
            "Provider {}\n  build: {}\n  priority: {}\n  origin: {}\n  config: {}\n  declared at: {}\n",
            provider.name,
            product.manifest.build_id,
            provider.priority,
            json(&provider.origin),
            json(&provider.config),
            provider.declared_at
        );
        output.push_str(&render_list(
            "Preparations",
            product
                .manifest
                .preparations
                .iter()
                .filter(|operation| operation.provider == provider.name)
                .map(|operation| {
                    format!(
                        "{}: {} (elevated={})",
                        operation.identity, operation.description, operation.elevated
                    )
                }),
        ));
        return Ok(output);
    }
    if let Some(identity) = selector.strip_prefix("preparation:") {
        let operation = product
            .manifest
            .preparations
            .iter()
            .find(|operation| format!("{}:{}", operation.provider, operation.identity) == identity)
            .ok_or_else(|| {
                WombatError::configuration(format!(
                    "no preparation in build `{}` matches `{selector}`",
                    product.manifest.build_id
                ))
            })?;
        return Ok(format!(
            "Preparation {}:{}\n  build: {}\n  description: {}\n  elevated: {}\n  data: {}\n",
            operation.provider,
            operation.identity,
            product.manifest.build_id,
            operation.description,
            operation.elevated,
            json(&operation.data)
        ));
    }
    if let Some((kind, name)) = selector.split_once(':')
        && matches!(kind, "command" | "package")
    {
        let requirements = product
            .manifest
            .requirements
            .iter()
            .filter(|requirement| {
                let matches_kind = matches!(
                    (kind, requirement.kind),
                    ("command", crate::model::manifest::RequirementKind::Command)
                        | ("package", crate::model::manifest::RequirementKind::Package)
                );
                matches_kind
                    && requirement
                        .candidates
                        .iter()
                        .any(|candidate| candidate.name() == name)
            })
            .collect::<Vec<_>>();
        if requirements.is_empty() {
            return Err(WombatError::configuration(format!(
                "no requirement in build `{}` matches `{selector}`",
                product.manifest.build_id
            )));
        }
        return Ok(render_requirement_explanation(
            &product.manifest,
            selector,
            &requirements,
        ));
    }
    let matches = product
        .manifest
        .artifacts
        .iter()
        .filter(|artifact| artifact_aliases(artifact, current_home).contains(selector))
        .collect::<Vec<_>>();
    let artifact = match matches.as_slice() {
        [artifact] => *artifact,
        [] => {
            return Err(WombatError::configuration(format!(
                "no artifact in build `{}` matches `{selector}`",
                product.manifest.build_id
            )));
        }
        matches => {
            let candidates = matches
                .iter()
                .map(|artifact| format!("`{}` from `{}`", artifact.target.path, artifact.source))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(WombatError::configuration(format!(
                "artifact selector `{selector}` is ambiguous: {candidates}"
            )));
        }
    };
    Ok(render_explanation(&product.manifest, artifact, source_root))
}

pub fn compare(left: &Path, right: &Path) -> Result<String> {
    let left = open_build(left)?;
    let right = open_build(right)?;
    Ok(render_comparison(&left.manifest, &right.manifest))
}

fn render_section(
    manifest: &Manifest,
    journal: Option<&crate::execution::ladder::ExecutionJournal>,
    section: InspectSection,
) -> String {
    match section {
        InspectSection::Overview => format!(
            "Build {}\n  manifest: v{}\n  plan: {}\n  wombat: {}\n  target: {}/{}\n  sources: {}\n  inputs: {}\n  modules: {}\n  dependencies: {}\n  providers: {}\n  preparations: {}\n  requirements: {}\n  tasks: {}\n  artifact selections: {}\n  artifacts: {}\n",
            manifest.build_id,
            manifest.format_version,
            manifest.plan_id,
            manifest.wombat_version,
            manifest.target.platform.os.name.as_str(),
            manifest.target.platform.arch.as_str(),
            manifest.sources.len(),
            manifest.inputs.len(),
            manifest.modules.len(),
            manifest.dependencies.len(),
            manifest.providers.len(),
            manifest.preparations.len(),
            manifest.requirements.len(),
            manifest.tasks.len(),
            manifest.artifact_selections.len(),
            manifest.artifacts.len(),
        ),
        InspectSection::Inputs => render_list(
            "Inputs",
            manifest.inputs.iter().map(|input| {
                format!(
                    "{} = {} ({:?}, {:?})\n  declared at: {}",
                    input.name,
                    json(&input.value),
                    input.kind,
                    input.origin,
                    input.declared_at
                )
            }),
        ),
        InspectSection::Target => {
            let mut output = format!(
                "Target\n{}\n  origin: {:?}\n",
                indented_json(&manifest.target.platform),
                manifest.target.origin
            );
            if let Some(location) = &manifest.target.declared_at {
                output.push_str(&format!("  declared at: {location}\n"));
            }
            output.push_str(&render_list(
                "Observations",
                manifest.observations.iter().map(|observation| {
                    format!(
                        "{:?}.{} = {}",
                        observation.subject,
                        observation.path,
                        json(&observation.value)
                    )
                }),
            ));
            output
        }
        InspectSection::Modules => render_list(
            "Modules",
            manifest.modules.iter().map(|module| {
                format!(
                    "{}\n  source: {}\n  config: {}",
                    module.name,
                    module.source,
                    json(&module.config)
                )
            }),
        ),
        InspectSection::Dependencies => render_list(
            "Dependencies",
            manifest.dependencies.iter().map(|dependency| {
                format!(
                    "{:?} {} -> {}\n  declared at: {}",
                    dependency.kind, dependency.from, dependency.to, dependency.declared_at
                )
            }),
        ),
        InspectSection::Providers => {
            let mut output = render_list(
                "Providers",
                manifest.providers.iter().map(|provider| {
                    format!(
                        "{} (priority {})\n  origin: {}\n  config: {}\n  declared at: {}",
                        provider.name,
                        provider.priority,
                        match &provider.origin {
                            crate::model::manifest::ProviderOrigin::Builtin {
                                contract_version,
                            } => {
                                format!("built-in contract v{contract_version}")
                            }
                            crate::model::manifest::ProviderOrigin::Custom {
                                entrypoint,
                                files,
                            } => {
                                format!("custom {entrypoint}, {} files", files.len())
                            }
                        },
                        json(&provider.config),
                        provider.declared_at
                    )
                }),
            );
            output.push_str(&render_list(
                "Preparations",
                manifest.preparations.iter().map(|operation| {
                    format!(
                        "{}:{}\n  description: {}\n  elevated: {}\n  data: {}",
                        operation.provider,
                        operation.identity,
                        operation.description,
                        operation.elevated,
                        json(&operation.data)
                    )
                }),
            ));
            output
        }
        InspectSection::Requirements => render_list(
            "Requirements",
            manifest.requirements.iter().map(render_requirement),
        ),
        InspectSection::Ladder => render_ladder(&manifest.ladder),
        InspectSection::Scripts => {
            let mut output = render_scripts(&manifest.scripts);
            let identities = manifest
                .scripts
                .iter()
                .map(|script| script.identity.as_str())
                .collect::<BTreeSet<_>>();
            match journal {
                Some(journal) => output.push_str(&render_list(
                    "Materialisation outcomes",
                    journal
                        .actions
                        .iter()
                        .filter(|action| identities.contains(action.identity.as_str()))
                        .map(|action| {
                            format!(
                                "{}\n  rung: {}\n  status: {:?}\n  reason: {}",
                                action.identity, action.rung, action.status, action.reason
                            )
                        }),
                )),
                None => output.push_str(
                    "Materialisation outcomes\n  unavailable (no matching execution journal)\n",
                ),
            }
            output
        }
        InspectSection::Tasks => render_list(
            "Tasks",
            manifest.tasks.iter().map(|task| {
                format!(
                    "{}\n  entrypoint: {}\n  runner: {:?}\n  outputs: {}\n  declared at: {}",
                    task.identity,
                    task.entrypoint,
                    task.runner.family_name(),
                    task.outputs.len(),
                    task.declared_at
                )
            }),
        ),
        InspectSection::Artifacts => {
            render_list(
                "Artifact selections",
                manifest.artifact_selections.iter().map(render_selection),
            ) + &render_list(
                "Artifacts",
                manifest.artifacts.iter().map(|artifact| {
                    format!(
                        "{}\n  owner: {}\n  source: {}\n  production: {}\n  digest: {}",
                        artifact.target.path,
                        artifact.owner,
                        artifact.source,
                        production_name(&artifact.production),
                        artifact.content.digest
                    )
                }),
            )
        }
        InspectSection::Sources => render_list(
            "Sources",
            manifest
                .sources
                .iter()
                .map(|source| format!("{}\n  digest: {}", source.path, source.digest)),
        ),
        InspectSection::Observations => {
            render_observations(&manifest.observations, &manifest.process_observations)
        }
    }
}

fn render_requirement(requirement: &crate::model::manifest::Requirement) -> String {
    let selected = &requirement.candidates[requirement.selected as usize];
    format!(
        "{}:{}\n  owner: {}\n  choice: {:?}\n  when: {}\n  provider: {}\n  binding: {}\n  candidates: {}\n  declared at: {}",
        match requirement.kind {
            crate::model::manifest::RequirementKind::Command => "command",
            crate::model::manifest::RequirementKind::Package => "package",
        },
        selected.name(),
        requirement.owner,
        requirement.choice,
        requirement.when.id(),
        requirement.binding.provider,
        requirement.binding.identity,
        requirement.candidates.len(),
        requirement.declared_at
    )
}

fn render_selection(selection: &crate::model::manifest::ArtifactSelection) -> String {
    format!(
        "{}\n  owner: {}\n  expanded: {}\n  physical: {}/{}\n  kind: {:?}\n  target: {}\n  matches: {}\n  skipped unallocated: {}\n  declared at: {}",
        selection.declared,
        selection.owner,
        selection.expanded,
        selection.source_base,
        selection.physical,
        selection.kind,
        selection
            .explicit_target
            .as_deref()
            .or(selection.source_base_target.as_deref())
            .unwrap_or("unallocated"),
        selection.matches.len(),
        selection.skipped_unallocated.len(),
        selection.declared_at,
    )
}

fn render_requirement_explanation(
    manifest: &Manifest,
    selector: &str,
    requirements: &[&crate::model::manifest::Requirement],
) -> String {
    let mut output = format!("Requirement {selector}\n  build: {}\n", manifest.build_id);
    for requirement in requirements {
        output.push_str(&format!(
            "  {}\n",
            render_requirement(requirement).replace('\n', "\n  ")
        ));
        output.push_str(&format!("  attempts: {}\n", json(&requirement.attempts)));
        output.push_str(&format!(
            "  publications: {}\n",
            json(&requirement.binding.publications)
        ));
    }
    output
}

fn render_list(title: &str, items: impl IntoIterator<Item = String>) -> String {
    let items = items.into_iter().collect::<Vec<_>>();
    if items.is_empty() {
        return format!("{title}\n  none\n");
    }
    let mut output = format!("{title}\n");
    for item in items {
        for (index, line) in item.lines().enumerate() {
            output.push_str(if index == 0 { "  " } else { "    " });
            output.push_str(line);
            output.push('\n');
        }
    }
    output
}

fn render_explanation(
    manifest: &Manifest,
    artifact: &Artifact,
    source_root: Option<&Path>,
) -> String {
    let module = manifest
        .modules
        .iter()
        .find(|module| module.name == artifact.owner);
    let mut output = format!(
        "Artifact {}\n  build: {}\n  owner: {}\n  source: {}\n  source origin: {}\n  production: {}\n  target inference: {}\n  content: {} bytes, {}, executable={}\n  declared at: {}\n",
        artifact.target.path,
        manifest.build_id,
        artifact.owner,
        artifact.source,
        json(&artifact.source_origin),
        production_name(&artifact.production),
        json(&artifact.target.origin),
        artifact.content.size,
        artifact.content.digest,
        artifact.content.executable,
        artifact.declared_at,
    );
    if let Some(module) = module {
        output.push_str(&format!(
            "  module source: {}\n  module config: {}\n",
            module.source,
            json(&module.config)
        ));
    }
    if let Production::Template {
        renderer,
        source_digest,
        context,
    } = &artifact.production
    {
        output.push_str(&format!(
            "  renderer: {}-v{}\n  template source: {}\n  template context: {}\n",
            renderer.name,
            renderer.contract_version,
            source_digest,
            json(context)
        ));
    }
    let relationships = manifest
        .dependencies
        .iter()
        .filter(|dependency| dependency.from == artifact.owner || dependency.to == artifact.owner)
        .map(|dependency| {
            format!(
                "{:?} {} -> {} at {}",
                dependency.kind, dependency.from, dependency.to, dependency.declared_at
            )
        });
    output.push_str(&render_list("Relationships", relationships));
    output.push_str(&source_freshness(
        manifest,
        &artifact.declared_at.primary.source,
        artifact.declared_at.primary.line,
        source_root,
    ));
    output
}

fn source_freshness(
    manifest: &Manifest,
    source: &str,
    line: Option<u32>,
    source_root: Option<&Path>,
) -> String {
    let Some(recorded) = manifest.sources.iter().find(|entry| entry.path == source) else {
        return "Source excerpt\n  unavailable: declaration source is not catalogued\n".to_string();
    };
    let Some(root) = source_root else {
        return "Source excerpt\n  unavailable: source repository is not available\n".to_string();
    };
    let path = portable_join(root, source);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(_) => return "Source excerpt\n  unavailable: recorded source is missing\n".to_string(),
    };
    if digest(&bytes) != recorded.digest {
        return "Source excerpt\n  unavailable: current source differs from this build\n"
            .to_string();
    }
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(_) => {
            return "Source excerpt\n  unavailable: current source is not UTF-8\n".to_string();
        }
    };
    let Some(line_number) = line else {
        return "Source excerpt\n  unavailable: no line was recorded\n".to_string();
    };
    let Some(source_line) = text.lines().nth(line_number.saturating_sub(1) as usize) else {
        return "Source excerpt\n  unavailable: recorded line is outside current source\n"
            .to_string();
    };
    format!("Source excerpt\n  {source}:{line_number}\n  {line_number} | {source_line}\n")
}

fn artifact_aliases(artifact: &Artifact, home: Option<&Path>) -> BTreeSet<String> {
    let mut aliases = BTreeSet::from([
        artifact.target.path.clone(),
        artifact.target.path.clone(),
        artifact.source.clone(),
    ]);
    let logical = artifact.target.path.clone();
    aliases.insert(logical.clone());
    if let Some(home) = home {
        aliases.insert(home.join(logical).to_string_lossy().into_owned());
    }
    aliases
}
