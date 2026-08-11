use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::build::open_build;
use crate::manifest::{Artifact, Manifest, Production, TargetAnchor};
use crate::{Result, WombatError};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InspectSection {
    Overview,
    Inputs,
    Target,
    Modules,
    Dependencies,
    Artifacts,
    Sources,
}

pub fn inspect(build_dir: &Path, section: InspectSection) -> Result<String> {
    let product = open_build(build_dir)?;
    Ok(render_section(&product.manifest, section))
}

pub fn explain(
    build_dir: &Path,
    selector: &str,
    source_root: Option<&Path>,
    current_home: Option<&Path>,
) -> Result<String> {
    let product = open_build(build_dir)?;
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
                .map(|artifact| format!("`{}` from `{}`", artifact.target.display, artifact.source))
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

fn render_section(manifest: &Manifest, section: InspectSection) -> String {
    match section {
        InspectSection::Overview => format!(
            "Build {}\n  manifest: v{}\n  wombat: {}\n  target: {}/{}\n  sources: {}\n  inputs: {}\n  modules: {}\n  dependencies: {}\n  artifacts: {}\n",
            manifest.build_id,
            manifest.format_version,
            manifest.wombat_version,
            manifest.target.platform.os.name.as_str(),
            manifest.target.platform.arch.as_str(),
            manifest.sources.len(),
            manifest.inputs.len(),
            manifest.modules.len(),
            manifest.dependencies.len(),
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
        InspectSection::Artifacts => render_list(
            "Artifacts",
            manifest.artifacts.iter().map(|artifact| {
                format!(
                    "{}\n  owner: {}\n  source: {}\n  production: {}\n  digest: {}",
                    artifact.target.display,
                    artifact.owner,
                    artifact.source,
                    production_name(&artifact.production),
                    artifact.content.digest
                )
            }),
        ),
        InspectSection::Sources => render_list(
            "Sources",
            manifest
                .sources
                .iter()
                .map(|source| format!("{}\n  digest: {}", source.path, source.digest)),
        ),
    }
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
        artifact.target.display,
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
        artifact.target.display.clone(),
        artifact.target.path.clone(),
        artifact.source.clone(),
    ]);
    let logical = match artifact.target.anchor {
        TargetAnchor::Config => format!(".config/{}", artifact.target.path),
        TargetAnchor::Home => artifact.target.path.clone(),
    };
    aliases.insert(logical.clone());
    if let Some(home) = home {
        aliases.insert(home.join(logical).to_string_lossy().into_owned());
    }
    aliases
}

fn render_comparison(left: &Manifest, right: &Manifest) -> String {
    if left == right {
        return format!("Products are identical: {}\n", left.build_id);
    }
    let mut output = format!(
        "Product comparison\n  left: {}\n  right: {}\n",
        left.build_id, right.build_id
    );
    compare_map(
        &mut output,
        "Sources",
        keyed(&left.sources, |source| source.path.clone()),
        keyed(&right.sources, |source| source.path.clone()),
    );
    compare_map(
        &mut output,
        "Inputs",
        keyed(&left.inputs, |input| input.name.clone()),
        keyed(&right.inputs, |input| input.name.clone()),
    );
    if left.target != right.target {
        output.push_str(&format!(
            "Target\n  - {}\n  + {}\n",
            json(&left.target),
            json(&right.target)
        ));
    }
    compare_map(
        &mut output,
        "Observations",
        keyed(&left.observations, |observation| {
            format!("{:?}.{}", observation.subject, observation.path)
        }),
        keyed(&right.observations, |observation| {
            format!("{:?}.{}", observation.subject, observation.path)
        }),
    );
    compare_map(
        &mut output,
        "Modules",
        keyed(&left.modules, |module| module.name.clone()),
        keyed(&right.modules, |module| module.name.clone()),
    );
    compare_map(
        &mut output,
        "Dependencies",
        keyed(&left.dependencies, |dependency| {
            format!(
                "{:?}:{}->{}@{}",
                dependency.kind,
                dependency.from,
                dependency.to,
                json(&dependency.declared_at)
            )
        }),
        keyed(&right.dependencies, |dependency| {
            format!(
                "{:?}:{}->{}@{}",
                dependency.kind,
                dependency.from,
                dependency.to,
                json(&dependency.declared_at)
            )
        }),
    );
    compare_map(
        &mut output,
        "Artifacts",
        keyed(&left.artifacts, |artifact| artifact.target.display.clone()),
        keyed(&right.artifacts, |artifact| artifact.target.display.clone()),
    );
    output
}

fn keyed<T: Serialize>(
    values: &[T],
    key: impl Fn(&T) -> String,
) -> BTreeMap<String, serde_json::Value> {
    values
        .iter()
        .map(|value| {
            (
                key(value),
                serde_json::to_value(value).expect("manifest values serialize"),
            )
        })
        .collect()
}

fn compare_map(
    output: &mut String,
    title: &str,
    left: BTreeMap<String, serde_json::Value>,
    right: BTreeMap<String, serde_json::Value>,
) {
    let keys = left
        .keys()
        .chain(right.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let changed = keys
        .into_iter()
        .filter(|key| left.get(key) != right.get(key))
        .collect::<Vec<_>>();
    if changed.is_empty() {
        return;
    }
    output.push_str(title);
    output.push('\n');
    for key in changed {
        match (left.get(&key), right.get(&key)) {
            (Some(left), Some(right)) => {
                output.push_str(&format!(
                    "  Change {key}\n    - {}\n    + {}\n",
                    json(left),
                    json(right)
                ));
            }
            (Some(left), None) => {
                output.push_str(&format!("  Remove {key}\n    - {}\n", json(left)));
            }
            (None, Some(right)) => {
                output.push_str(&format!("  Add {key}\n    + {}\n", json(right)));
            }
            (None, None) => unreachable!(),
        }
    }
}

fn production_name(production: &Production) -> &'static str {
    match production {
        Production::Static => "static",
        Production::Template { .. } => "template",
    }
}

fn json(value: &impl Serialize) -> String {
    serde_json::to_string(value).expect("manifest values serialize")
}

fn indented_json(value: &impl Serialize) -> String {
    serde_json::to_string_pretty(value)
        .expect("manifest values serialize")
        .lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn portable_join(root: &Path, relative: &str) -> PathBuf {
    relative
        .split('/')
        .fold(root.to_path_buf(), |path, component| path.join(component))
}

fn digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::from("sha256:");
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to a string cannot fail");
    }
    output
}
