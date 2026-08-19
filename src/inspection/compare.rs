//! Deterministic semantic comparison of two sealed products.

use super::*;

pub(super) fn render_comparison(left: &Manifest, right: &Manifest) -> String {
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
        "Template helpers",
        keyed(&left.template_helpers, |pack| {
            format!("{}:{}", pack.module, pack.prefix)
        }),
        keyed(&right.template_helpers, |pack| {
            format!("{}:{}", pack.module, pack.prefix)
        }),
    );
    compare_map(
        &mut output,
        "Providers",
        keyed(&left.providers, |provider| provider.name.clone()),
        keyed(&right.providers, |provider| provider.name.clone()),
    );
    compare_map(
        &mut output,
        "Requirements",
        keyed(&left.requirements, |requirement| {
            format!(
                "{:?}:{}@{}",
                requirement.kind,
                requirement.candidates[requirement.selected as usize].name(),
                requirement.declared_at
            )
        }),
        keyed(&right.requirements, |requirement| {
            format!(
                "{:?}:{}@{}",
                requirement.kind,
                requirement.candidates[requirement.selected as usize].name(),
                requirement.declared_at
            )
        }),
    );
    compare_map(
        &mut output,
        "Prerequisites",
        keyed(&left.prerequisites, |prerequisite| {
            format!("{}:{}", prerequisite.provider, prerequisite.identity)
        }),
        keyed(&right.prerequisites, |prerequisite| {
            format!("{}:{}", prerequisite.provider, prerequisite.identity)
        }),
    );
    compare_map(
        &mut output,
        "Preparations",
        keyed(&left.preparations, |operation| {
            format!("{}:{}", operation.provider, operation.identity)
        }),
        keyed(&right.preparations, |operation| {
            format!("{}:{}", operation.provider, operation.identity)
        }),
    );
    compare_map(
        &mut output,
        "Artifact selections",
        keyed(&left.artifact_selections, |selection| {
            format!(
                "{}:{}@{}",
                selection.owner, selection.declared, selection.declared_at
            )
        }),
        keyed(&right.artifact_selections, |selection| {
            format!(
                "{}:{}@{}",
                selection.owner, selection.declared, selection.declared_at
            )
        }),
    );
    if left.artifact_policy != right.artifact_policy {
        output.push_str(&format!(
            "Artifact policy\n  - {}\n  + {}\n",
            json(&left.artifact_policy),
            json(&right.artifact_policy)
        ));
    }
    compare_map(
        &mut output,
        "Artifact notices",
        keyed(&left.artifact_notices, |notice| {
            format!(
                "{}:{}@{}",
                notice.owner, notice.selector, notice.declared_at
            )
        }),
        keyed(&right.artifact_notices, |notice| {
            format!(
                "{}:{}@{}",
                notice.owner, notice.selector, notice.declared_at
            )
        }),
    );
    compare_map(
        &mut output,
        "Artifacts",
        // Target scopes have disjoint textual grammars, so their displayed
        // paths remain unambiguous here. Keeping the internal scoped key out
        // of human comparison preserves the familiar artifact vocabulary.
        keyed(&left.artifacts, |artifact| artifact.target.path.clone()),
        keyed(&right.artifacts, |artifact| artifact.target.path.clone()),
    );
    output
}

pub(super) fn keyed<T: Serialize>(
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

pub(super) fn compare_map(
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

pub(super) fn production_name(production: &Production) -> &'static str {
    match production {
        Production::Static => "static",
        Production::Template { .. } => "template",
        Production::GeneratedLua { .. } => "generated Lua",
        Production::Task { .. } => "task",
    }
}

pub(super) fn json(value: &impl Serialize) -> String {
    serde_json::to_string(value).expect("manifest values serialize")
}

pub(super) fn indented_json(value: &impl Serialize) -> String {
    serde_json::to_string_pretty(value)
        .expect("manifest values serialize")
        .lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn portable_join(root: &Path, relative: &str) -> PathBuf {
    relative
        .split('/')
        .fold(root.to_path_buf(), |path, component| path.join(component))
}

pub(super) fn digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::from("sha256:");
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to a string cannot fail");
    }
    output
}
