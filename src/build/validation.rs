//! Validation of sealed manifests, provider payloads, and published trees.

use super::materialisation::compute_build_id;
use super::*;

pub(super) fn verify_product(root: &Path) -> Result<Manifest> {
    let manifest_path = root.join("manifest.json");
    ensure_plain_file(&manifest_path)?;
    let contents = fs::read_to_string(&manifest_path)
        .map_err(|error| WombatError::io(&manifest_path, error))?;
    let manifest: Manifest = serde_json::from_str(&contents)?;
    validate_manifest(&manifest)?;
    crate::execution::script::verify_payloads(
        root,
        &manifest.scripts,
        crate::execution::script::PayloadKind::Product,
    )?;
    let expected_id = compute_build_id(&manifest)?;
    if manifest.build_id != expected_id {
        return Err(WombatError::configuration(format!(
            "build ID mismatch in `{}`: recorded `{}`, computed `{expected_id}`",
            manifest_path.display(),
            manifest.build_id
        )));
    }
    verify_tree(&root.join("tree"), &manifest)?;
    verify_provider_payloads(root, &manifest)?;
    Ok(manifest)
}

pub(crate) fn validate_manifest(manifest: &Manifest) -> Result<()> {
    if manifest.format_version != MANIFEST_FORMAT_VERSION {
        return Err(WombatError::configuration(format!(
            "unsupported manifest format version {}; expected {MANIFEST_FORMAT_VERSION}; rebuild this product with the current Wombat",
            manifest.format_version
        )));
    }
    if manifest.wombat_version != WOMBAT_VERSION {
        return Err(WombatError::configuration(format!(
            "build was produced by Wombat {}, but this is Wombat {WOMBAT_VERSION}",
            manifest.wombat_version
        )));
    }
    manifest.ladder.validate()?;
    validate_sha256(&manifest.project_identity, "manifest project identity")?;
    crate::model::plan::validate_actions(&manifest.ladder, &manifest.tasks, &manifest.scripts)?;
    validate_sha256(&manifest.plan_id, "manifest build plan identity")?;
    if !manifest
        .sources
        .windows(2)
        .all(|pair| pair[0].path < pair[1].path)
    {
        return Err(WombatError::configuration(
            "manifest Lua sources are not uniquely sorted",
        ));
    }
    let source_paths = manifest
        .sources
        .iter()
        .map(|source| source.path.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    for source in &manifest.sources {
        validate_relative_path(&source.path, "manifest Lua source path")?;
        validate_sha256(&source.digest, "manifest Lua source digest")?;
    }
    crate::model::context::TargetPlatform::from_frozen(&manifest.target.platform.to_frozen())?;
    match (&manifest.target.origin, &manifest.target.declared_at) {
        (crate::model::context::TargetOrigin::HostDefault, None)
        | (crate::model::context::TargetOrigin::RootOverride, Some(_)) => {}
        _ => {
            return Err(WombatError::configuration(
                "manifest target origin and declaration location are inconsistent",
            ));
        }
    }
    if let Some(trace) = &manifest.target.declared_at {
        validate_source_trace(trace, &source_paths, "manifest target declaration")?;
    }
    if !manifest
        .inputs
        .windows(2)
        .all(|pair| pair[0].name < pair[1].name)
    {
        return Err(WombatError::configuration(
            "manifest build inputs are not uniquely sorted",
        ));
    }
    for input in &manifest.inputs {
        let mut name = input.name.bytes();
        if !name
            .next()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
            || !name.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return Err(WombatError::configuration(format!(
                "manifest build input name `{}` is invalid",
                input.name
            )));
        }
        validate_source_trace(
            &input.declared_at,
            &source_paths,
            "manifest input declaration",
        )?;
        match input.kind {
            crate::model::manifest::BuildInputKind::Flag
                if !matches!(input.value, crate::model::frozen::FrozenValue::Boolean(_)) =>
            {
                return Err(WombatError::configuration(
                    "manifest flag input is not boolean",
                ));
            }
            crate::model::manifest::BuildInputKind::Choice
            | crate::model::manifest::BuildInputKind::String
            | crate::model::manifest::BuildInputKind::Target
                if !matches!(input.value, crate::model::frozen::FrozenValue::String(_)) =>
            {
                return Err(WombatError::configuration(
                    "manifest textual input is not a string",
                ));
            }
            crate::model::manifest::BuildInputKind::Integer
                if !matches!(input.value, crate::model::frozen::FrozenValue::Integer(_)) =>
            {
                return Err(WombatError::configuration(
                    "manifest integer input is not an integer",
                ));
            }
            _ => {}
        }
        if input.kind == crate::model::manifest::BuildInputKind::Target
            && let crate::model::frozen::FrozenValue::String(value) = &input.value
        {
            let parsed = crate::model::context::TargetPlatform::parse_compact(value)?;
            if parsed.compact() != *value {
                return Err(WombatError::configuration(
                    "manifest target input is not canonical",
                ));
            }
        }
    }
    if !manifest.observations.windows(2).all(|pair| {
        (pair[0].subject, pair[0].path.as_str()) < (pair[1].subject, pair[1].path.as_str())
    }) {
        return Err(WombatError::configuration(
            "manifest observations are not uniquely sorted",
        ));
    }
    if manifest.observations.iter().any(|observation| {
        observation.path.is_empty()
            || observation.path.split('.').any(|component| {
                component.is_empty()
                    || !component
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            })
    }) {
        return Err(WombatError::configuration(
            "manifest contains an invalid observation path",
        ));
    }
    if !manifest
        .modules
        .windows(2)
        .all(|pair| pair[0].name < pair[1].name)
    {
        return Err(WombatError::configuration(
            "manifest modules are not uniquely sorted",
        ));
    }
    for module in &manifest.modules {
        validate_relative_path(&module.source, "manifest module source")?;
        if !source_paths.contains(module.source.as_str()) {
            return Err(WombatError::configuration(format!(
                "manifest module `{}` references uncatalogued source `{}`",
                module.name, module.source
            )));
        }
        if let Some(base) = &module.source_base {
            let compiled = crate::model::selection::compile_selector(&base.declared, base.hidden)?;
            let expected_physical = if compiled.physical == "." {
                "src".to_string()
            } else {
                format!("src/{}", compiled.physical)
            };
            if base.expanded != compiled.expanded || base.physical != expected_physical {
                return Err(WombatError::configuration(format!(
                    "manifest module `{}` has inconsistent source-base projection",
                    module.name
                )));
            }
            let projection = if compiled.physical == "." {
                crate::model::manifest::SourceProjection {
                    physical: String::new(),
                    logical: String::new(),
                    allocated: true,
                    hidden: base.hidden,
                    components: Vec::new(),
                }
            } else {
                crate::model::selection::project_physical(&compiled.physical, base.hidden)?
            };
            if base.logical != projection.logical || (base.target.is_none() && projection.allocated)
            {
                return Err(WombatError::configuration(format!(
                    "manifest module `{}` has inconsistent logical or target source base",
                    module.name
                )));
            }
            if let Some(target) = &base.target {
                parse_explicit_target_root(target)?;
            }
        }
    }
    if !manifest
        .dependencies
        .windows(2)
        .all(|pair| pair[0] < pair[1])
    {
        return Err(WombatError::configuration(
            "manifest dependencies are not uniquely sorted",
        ));
    }
    for dependency in &manifest.dependencies {
        validate_source_trace(
            &dependency.declared_at,
            &source_paths,
            "manifest dependency declaration",
        )?;
    }
    validate_provider_scope(
        &manifest.providers,
        &manifest.requirements,
        &manifest.preparations,
        &source_paths,
        "target",
    )?;
    for requirement in &manifest.requirements {
        if !manifest.ladder.contains(&requirement.when)
            || manifest.ladder.is_container(&requirement.when)
        {
            return Err(WombatError::configuration(format!(
                "manifest requirement targets invalid rung `{}`",
                requirement.when
            )));
        }
    }
    validate_tasks(manifest, &source_paths)?;
    for script in &manifest.scripts {
        validate_source_trace(
            &script.declared_at,
            &source_paths,
            "manifest script declaration",
        )?;
        if !script
            .payloads
            .windows(2)
            .all(|pair| pair[0].relative < pair[1].relative)
        {
            return Err(WombatError::configuration(format!(
                "manifest script `{}` payloads are not uniquely sorted",
                script.identity
            )));
        }
    }
    validate_artifact_metadata(
        manifest.artifact_policy,
        &manifest.artifact_notices,
        &manifest.artifact_selections,
        &source_paths,
    )?;
    if !manifest.artifacts.windows(2).all(|pair| {
        pair[0]
            .target
            .key()
            .cmp(pair[1].target.key())
            .then_with(|| pair[0].owner.cmp(&pair[1].owner))
            .then_with(|| pair[0].source.cmp(&pair[1].source))
            .then_with(|| pair[0].declared_at.cmp(&pair[1].declared_at))
            .is_lt()
    }) {
        return Err(WombatError::configuration(
            "manifest artifacts are not uniquely sorted",
        ));
    }
    for artifact in &manifest.artifacts {
        validate_relative_path(&artifact.source, "manifest artifact source")?;
        validate_source_trace(
            &artifact.declared_at,
            &source_paths,
            "manifest artifact declaration",
        )?;
        validate_relative_path(&artifact.target.path, "manifest target path")?;
        if let Some(projection) = &artifact.source_projection {
            if projection.physical != artifact.source {
                return Err(WombatError::configuration(
                    "manifest source projection does not identify its artifact source",
                ));
            }
            if projection.components.is_empty() {
                return Err(WombatError::configuration(
                    "manifest source projection requires parsed components",
                ));
            }
            let relative = projection
                .components
                .iter()
                .map(|component| component.physical.as_str())
                .collect::<Vec<_>>()
                .join("/");
            let expected = crate::model::selection::project_physical(&relative, projection.hidden)?;
            if expected.logical != projection.logical
                || expected.allocated != projection.allocated
                || expected.hidden != projection.hidden
                || expected.components != projection.components
                || !artifact.source.ends_with(&relative)
            {
                return Err(WombatError::configuration(
                    "manifest source projection is internally inconsistent",
                ));
            }
        }
        match &artifact.source_origin {
            SourceOrigin::Direct { declared, .. } => {
                validate_declared_source(declared)?;
                if declared == "." {
                    return Err(WombatError::configuration(
                        "manifest direct artifact source must identify a file",
                    ));
                }
                let expected_source = artifact
                    .source_projection
                    .as_ref()
                    .map(|value| value.physical.as_str())
                    .ok_or_else(|| {
                        WombatError::configuration(
                            "manifest direct source is missing source projection",
                        )
                    })?;
                if artifact.source != expected_source {
                    return Err(WombatError::configuration(format!(
                        "manifest direct source `{}` does not match declared source `{expected_source}`",
                        artifact.source
                    )));
                }
            }
            SourceOrigin::Directory {
                declared,
                root,
                relative,
                ..
            } => {
                validate_declared_source(declared)?;
                validate_relative_path(root, "manifest directory source root")?;
                validate_relative_path(relative, "manifest directory relative path")?;
                let expected_source = artifact
                    .source_projection
                    .as_ref()
                    .map(|value| value.physical.as_str())
                    .ok_or_else(|| {
                        WombatError::configuration(
                            "manifest directory source is missing source projection",
                        )
                    })?;
                if artifact.source != expected_source {
                    return Err(WombatError::configuration(format!(
                        "manifest directory source `{}` does not match `{expected_source}`",
                        artifact.source
                    )));
                }
            }
            SourceOrigin::Generated { name } => {
                validate_relative_path(name, "manifest generated artifact name")?;
                if !matches!(artifact.production, Production::GeneratedLua { .. }) {
                    return Err(WombatError::configuration(
                        "manifest generated source must use generated Lua production",
                    ));
                }
            }
            SourceOrigin::Task { identity, relative } => {
                if identity.is_empty() {
                    return Err(WombatError::configuration(
                        "manifest task artifact identity must not be empty",
                    ));
                }
                validate_relative_path(relative, "manifest task output path")?;
                if !matches!(artifact.production, Production::Task { .. }) {
                    return Err(WombatError::configuration(
                        "manifest task source must use task production",
                    ));
                }
            }
        }
        match &artifact.production {
            Production::Static => {}
            Production::Template {
                renderer,
                source_digest,
                context,
            } => {
                if renderer.name != TEMPLATE_RENDERER_NAME
                    || renderer.contract_version != TEMPLATE_CONTRACT_VERSION
                {
                    return Err(WombatError::configuration(format!(
                        "unsupported template renderer contract `{}-v{}`",
                        renderer.name, renderer.contract_version
                    )));
                }
                if source_digest.len() != 71
                    || !source_digest.starts_with("sha256:")
                    || !source_digest[7..]
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit())
                {
                    return Err(WombatError::configuration(
                        "manifest template source digest is not a SHA-256 identity",
                    ));
                }
                if !matches!(context, crate::model::frozen::FrozenValue::Map(_)) {
                    return Err(WombatError::configuration(
                        "manifest template context must be a map",
                    ));
                }
            }
            Production::GeneratedLua { contract_version } => {
                if *contract_version != 1 {
                    return Err(WombatError::configuration(
                        "unsupported generated Lua production contract",
                    ));
                }
            }
            Production::Task {
                contract_version,
                identity,
                output,
            } => {
                if *contract_version != 1 || identity.is_empty() {
                    return Err(WombatError::configuration(
                        "unsupported or invalid task production contract",
                    ));
                }
                validate_relative_path(output, "manifest task production output")?;
                if !matches!(
                    &artifact.source_origin,
                    SourceOrigin::Task {
                        identity: source_identity,
                        relative,
                    } if source_identity == identity && relative == output
                ) {
                    return Err(WombatError::configuration(
                        "manifest task production does not match its source origin",
                    ));
                }
            }
        }
        let expected_display = display_target(&artifact.target.path);
        debug_assert_eq!(expected_display, artifact.target.path);
        match &artifact.target.origin {
            TargetOrigin::Explicit { declared } => {
                let parsed = parse_explicit_target(declared)?;
                if parsed.path != artifact.target.path {
                    return Err(WombatError::configuration(format!(
                        "manifest explicit target `{declared}` does not match its resolved target"
                    )));
                }
            }
            TargetOrigin::Inferred { source } => {
                if source.is_empty() {
                    return Err(WombatError::configuration(
                        "manifest inferred target requires source provenance",
                    ));
                }
            }
            TargetOrigin::DirectoryExplicit { declared, relative } => {
                let root = parse_explicit_target_root(declared)?;
                let parsed = expand_target_root(&root, relative)?;
                if parsed.path != artifact.target.path {
                    return Err(WombatError::configuration(format!(
                        "manifest directory target `{declared}` plus `{relative}` does not match its resolved target"
                    )));
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_artifact_metadata(
    policy: crate::model::manifest::ArtifactPolicy,
    notices: &[crate::model::manifest::ArtifactNotice],
    selections: &[crate::model::manifest::ArtifactSelection],
    source_paths: &std::collections::BTreeSet<&str>,
) -> Result<()> {
    use crate::model::manifest::{ArtifactNoticeKind, ArtifactSelectionKind, UnallocatedPolicy};

    for selection in selections {
        validate_source_trace(
            &selection.declared_at,
            source_paths,
            "artifact selection declaration",
        )?;
        validate_relative_path(&selection.source_base, "artifact selection source base")?;
        if selection.source_base != "src" && !selection.source_base.starts_with("src/") {
            return Err(WombatError::configuration(
                "artifact selection source base must live beneath `src/`",
            ));
        }
        if !selection.source_base_logical.is_empty() {
            validate_relative_path(
                &selection.source_base_logical,
                "artifact selection logical source base",
            )?;
        }
        if let Some(target) = &selection.source_base_target {
            parse_explicit_target_root(target)?;
        }
        if let Some(target) = &selection.explicit_target {
            parse_explicit_target_root(target)?;
        }
        let compiled =
            crate::model::selection::compile_selector(&selection.declared, selection.hidden)?;
        let relaxed_template = selection.kind == ArtifactSelectionKind::Exact
            && !compiled.physical.ends_with(".tmpl")
            && selection.physical == format!("{}.tmpl", compiled.physical)
            && selection.expanded == format!("{}.tmpl", compiled.expanded);
        if (!relaxed_template
            && (selection.physical != compiled.physical || selection.expanded != compiled.expanded))
            || selection.static_root != compiled.static_root
        {
            return Err(WombatError::configuration(
                "artifact selection does not match its declared selector",
            ));
        }
        let expected_kind = if compiled.glob {
            ArtifactSelectionKind::Glob
        } else if selection.kind == ArtifactSelectionKind::Glob {
            return Err(WombatError::configuration(
                "artifact selection kind is inconsistent with its selector",
            ));
        } else {
            selection.kind
        };
        if expected_kind != selection.kind {
            return Err(WombatError::configuration(
                "artifact selection kind is inconsistent with its selector",
            ));
        }
        if selection.kind == ArtifactSelectionKind::Exact && selection.allow_empty {
            return Err(WombatError::configuration(
                "exact artifact selections cannot allow an empty result",
            ));
        }
        for exclusion in &selection.exclusions {
            crate::model::selection::compile_selector(exclusion, selection.hidden)?;
        }
        for (label, paths) in [
            ("matches", &selection.matches),
            ("skipped sources", &selection.skipped_unallocated),
        ] {
            if !paths.windows(2).all(|pair| pair[0] < pair[1]) {
                return Err(WombatError::configuration(format!(
                    "artifact selection {label} are not uniquely sorted"
                )));
            }
            for path in paths {
                validate_relative_path(path, "artifact selection result path")?;
            }
        }
        if selection.matches.is_empty() && !selection.allow_empty {
            return Err(WombatError::configuration(
                "artifact selection without matches must explicitly allow an empty result",
            ));
        }
    }

    if policy.unallocated != UnallocatedPolicy::Warn && !notices.is_empty() {
        return Err(WombatError::configuration(
            "unallocated artifact notices require warning policy",
        ));
    }
    for notice in notices {
        if notice.kind != ArtifactNoticeKind::UnallocatedSkipped
            || notice.skipped.is_empty()
            || !notice.skipped.windows(2).all(|pair| pair[0] < pair[1])
        {
            return Err(WombatError::configuration(
                "manifest contains an invalid artifact notice",
            ));
        }
        validate_source_trace(
            &notice.declared_at,
            source_paths,
            "artifact notice declaration",
        )?;
        let matching = selections.iter().filter(|selection| {
            selection.owner == notice.owner
                && selection.declared == notice.selector
                && selection.declared_at == notice.declared_at
                && selection.skipped_unallocated == notice.skipped
        });
        if matching.count() != 1 {
            return Err(WombatError::configuration(
                "artifact notice does not identify exactly one source selection",
            ));
        }
    }
    Ok(())
}

fn validate_tasks(
    manifest: &Manifest,
    source_paths: &std::collections::BTreeSet<&str>,
) -> Result<()> {
    let mut identities = BTreeSet::new();
    for task in &manifest.tasks {
        if task.identity.is_empty() || !identities.insert(task.identity.as_str()) {
            return Err(WombatError::configuration(
                "manifest task identities must be non-empty and unique",
            ));
        }
        validate_relative_path(&task.entrypoint, "manifest task entrypoint")?;
        if !task.entrypoint.starts_with("tasks/") {
            return Err(WombatError::configuration(
                "manifest task entrypoints must live beneath `tasks/`",
            ));
        }
        validate_sha256(&task.entrypoint_digest, "manifest task entrypoint digest")?;
        validate_source_trace(&task.declared_at, source_paths, "manifest task declaration")?;
        if !matches!(task.params, crate::model::frozen::FrozenValue::Map(_))
            || task.runner.contract_version() != 1
            || task.runner.command().is_some_and(str::is_empty)
            || task.cache.revision.as_deref().is_some_and(str::is_empty)
        {
            return Err(WombatError::configuration(
                "manifest task contains invalid parameters, runner, or cache policy",
            ));
        }
        if !task
            .outputs
            .windows(2)
            .all(|pair| pair[0].relative < pair[1].relative)
        {
            return Err(WombatError::configuration(
                "manifest task outputs must be uniquely sorted",
            ));
        }
        if task.target_root.is_none() && !task.outputs.is_empty() {
            return Err(WombatError::configuration(
                "manifest task with outputs must have a target root",
            ));
        }
        for output in &task.outputs {
            validate_relative_path(&output.relative, "manifest task output")?;
            validate_sha256(&output.content.digest, "manifest task output digest")?;
            let matches = manifest
                .artifacts
                .iter()
                .filter(|artifact| {
                    matches!(
                        &artifact.production,
                        Production::Task { identity, output: relative, .. }
                            if identity == &task.identity && relative == &output.relative
                    ) && artifact.content == output.content
                })
                .count();
            if matches != 1 {
                return Err(WombatError::configuration(format!(
                    "manifest task `{}` output `{}` does not match exactly one artifact",
                    task.identity, output.relative
                )));
            }
        }
    }
    for artifact in &manifest.artifacts {
        if let Production::Task {
            identity, output, ..
        } = &artifact.production
            && !manifest.tasks.iter().any(|task| {
                task.identity == *identity
                    && task.outputs.iter().any(|candidate| {
                        candidate.relative == *output && candidate.content == artifact.content
                    })
            })
        {
            return Err(WombatError::configuration(format!(
                "manifest task artifact `{}` has no matching task output",
                artifact.target.path
            )));
        }
    }
    Ok(())
}

fn validate_provider_scope(
    providers: &[crate::model::manifest::Provider],
    requirements: &[crate::model::manifest::Requirement],
    preparations: &[crate::model::manifest::ProviderPreparation],
    source_paths: &std::collections::BTreeSet<&str>,
    scope: &str,
) -> Result<()> {
    let mut provider_names = BTreeSet::new();
    let mut payloads = BTreeSet::new();
    for (index, provider) in providers.iter().enumerate() {
        if !valid_provider_name(&provider.name) || !provider_names.insert(provider.name.as_str()) {
            return Err(WombatError::configuration(format!(
                "manifest provider name `{}` is invalid or duplicated",
                provider.name
            )));
        }
        if usize::try_from(provider.priority).ok() != Some(index) {
            return Err(WombatError::configuration(format!(
                "manifest {scope} provider priorities must be contiguous and ordered"
            )));
        }
        if !matches!(provider.config, crate::model::frozen::FrozenValue::Map(_)) {
            return Err(WombatError::configuration(format!(
                "manifest provider `{}` config must be a map",
                provider.name
            )));
        }
        validate_source_trace(
            &provider.declared_at,
            source_paths,
            "manifest provider declaration",
        )?;
        match &provider.origin {
            crate::model::manifest::ProviderOrigin::Builtin { contract_version } => {
                if !matches!(provider.name.as_str(), "brew" | "apt") || *contract_version != 1 {
                    return Err(WombatError::configuration(format!(
                        "unsupported built-in provider contract `{}-v{contract_version}`",
                        provider.name
                    )));
                }
            }
            crate::model::manifest::ProviderOrigin::Custom { entrypoint, files } => {
                validate_relative_path(entrypoint, "manifest provider entrypoint")?;
                if files.is_empty()
                    || !files
                        .windows(2)
                        .all(|pair| pair[0].payload < pair[1].payload)
                    || !files.iter().any(|file| file.payload == *entrypoint)
                {
                    return Err(WombatError::configuration(format!(
                        "manifest custom provider `{}` has an invalid payload catalog",
                        provider.name
                    )));
                }
                for file in files {
                    validate_relative_path(&file.source, "manifest provider source")?;
                    validate_relative_path(&file.payload, "manifest provider payload")?;
                    validate_sha256(&file.digest, "manifest provider payload digest")?;
                    if file.size == 0
                        || !source_paths.contains(file.source.as_str())
                        || !payloads.insert(file.payload.as_str())
                    {
                        return Err(WombatError::configuration(format!(
                            "manifest custom provider `{}` has an invalid or overlapping payload",
                            provider.name
                        )));
                    }
                }
            }
        }
    }

    for requirement in requirements {
        validate_source_trace(
            &requirement.declared_at,
            source_paths,
            "manifest requirement declaration",
        )?;
        if requirement.candidates.is_empty()
            || usize::try_from(requirement.selected)
                .ok()
                .is_none_or(|selected| selected >= requirement.candidates.len())
        {
            return Err(WombatError::configuration(
                "manifest requirement has an invalid selected candidate",
            ));
        }
        let selected = &requirement.candidates[requirement.selected as usize];
        let expected_kind = match selected {
            crate::model::manifest::RequirementCandidate::Command { .. } => {
                crate::model::manifest::RequirementKind::Command
            }
            crate::model::manifest::RequirementCandidate::Package { .. } => {
                crate::model::manifest::RequirementKind::Package
            }
        };
        if requirement.kind != expected_kind
            || requirement.candidates.iter().any(|candidate| {
                matches!(
                    candidate,
                    crate::model::manifest::RequirementCandidate::Command { .. }
                ) != matches!(
                    requirement.kind,
                    crate::model::manifest::RequirementKind::Command
                )
            })
        {
            return Err(WombatError::configuration(
                "manifest requirement candidates are not homogeneous",
            ));
        }
        for candidate in &requirement.candidates {
            let valid_name = match candidate {
                crate::model::manifest::RequirementCandidate::Command { name, .. } => {
                    valid_product_name(name)
                }
                crate::model::manifest::RequirementCandidate::Package { name, .. } => {
                    valid_package_name(name)
                }
            };
            if !valid_name
                || candidate
                    .minimum()
                    .is_some_and(|value| value.trim().is_empty())
            {
                return Err(WombatError::configuration(
                    "manifest requirement contains an invalid candidate",
                ));
            }
            if let crate::model::manifest::RequirementCandidate::Package {
                provider,
                publications,
                with,
                ..
            } = candidate
            {
                if !provider_names.contains(provider.as_str())
                    || !matches!(with, crate::model::frozen::FrozenValue::Map(_))
                {
                    return Err(WombatError::configuration(
                        "manifest package candidate has an invalid provider or options",
                    ));
                }
                validate_publications(publications)?;
            }
        }
        validate_publications(&requirement.binding.publications)?;
        if !provider_names.contains(requirement.binding.provider.as_str())
            || requirement.binding.identity.trim().is_empty()
            || !matches!(
                requirement.binding.data,
                crate::model::frozen::FrozenValue::Map(_)
            )
        {
            return Err(WombatError::configuration(
                "manifest requirement has an invalid selected binding",
            ));
        }
        let selected_attempts = requirement
            .attempts
            .iter()
            .filter(|attempt| {
                matches!(
                    attempt.outcome,
                    crate::model::manifest::ResolutionOutcome::Selected
                )
            })
            .count();
        if selected_attempts != 1
            || requirement.attempts.last().is_none_or(|attempt| {
                !matches!(
                    attempt.outcome,
                    crate::model::manifest::ResolutionOutcome::Selected
                ) || attempt.candidate != requirement.selected
                    || attempt.provider != requirement.binding.provider
            })
        {
            return Err(WombatError::configuration(
                "manifest requirement resolution attempts are inconsistent",
            ));
        }
        let mut expected_attempts = Vec::new();
        'candidates: for (candidate_index, candidate) in requirement.candidates.iter().enumerate() {
            for provider in providers {
                if let crate::model::manifest::RequirementCandidate::Package {
                    provider: required,
                    ..
                } = candidate
                    && provider.name != *required
                {
                    continue;
                }
                expected_attempts.push((candidate_index as u32, provider.name.as_str()));
                if candidate_index as u32 == requirement.selected
                    && provider.name == requirement.binding.provider
                {
                    break 'candidates;
                }
            }
        }
        if requirement.attempts.len() != expected_attempts.len()
            || requirement
                .attempts
                .iter()
                .zip(expected_attempts)
                .any(|(attempt, expected)| {
                    (attempt.candidate, attempt.provider.as_str()) != expected
                        || matches!(
                            &attempt.outcome,
                            crate::model::manifest::ResolutionOutcome::Unsupported { reason }
                                if reason.trim().is_empty()
                        )
                })
        {
            return Err(WombatError::configuration(
                "manifest requirement attempts do not follow candidate/provider precedence",
            ));
        }
        let expected_choice = if requirement.selected == 0 {
            match requirement.choice {
                crate::model::manifest::RequirementChoice::Required => {
                    crate::model::manifest::RequirementChoice::Required
                }
                _ => crate::model::manifest::RequirementChoice::Preferred,
            }
        } else {
            crate::model::manifest::RequirementChoice::Accepted
        };
        if requirement.choice != expected_choice {
            return Err(WombatError::configuration(
                "manifest requirement choice is inconsistent with its selection",
            ));
        }
    }
    let mut preparation_identities = BTreeSet::new();
    let mut previous_priority = None;
    for preparation in preparations {
        let priority = providers
            .iter()
            .find(|provider| provider.name == preparation.provider)
            .map(|provider| provider.priority)
            .ok_or_else(|| {
                WombatError::configuration("manifest preparation references an absent provider")
            })?;
        if previous_priority.is_some_and(|previous| priority < previous) {
            return Err(WombatError::configuration(
                "manifest preparations do not follow provider priority",
            ));
        }
        previous_priority = Some(priority);
        if preparation.identity.trim().is_empty()
            || preparation.description.trim().is_empty()
            || !matches!(preparation.data, crate::model::frozen::FrozenValue::Map(_))
            || !preparation_identities
                .insert((preparation.provider.as_str(), preparation.identity.as_str()))
        {
            return Err(WombatError::configuration(
                "manifest contains an invalid or duplicate provider preparation",
            ));
        }
    }
    Ok(())
}

fn valid_provider_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
        && name.as_bytes()[0].is_ascii_lowercase()
}

fn valid_product_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'+' | b'@')
        })
}

fn valid_package_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
}

fn validate_publications(publications: &crate::model::manifest::Publications) -> Result<()> {
    if !publications
        .commands
        .windows(2)
        .all(|pair| pair[0] < pair[1])
        || publications
            .commands
            .iter()
            .any(|command| !valid_product_name(command))
    {
        return Err(WombatError::configuration(
            "manifest command publications must be valid and uniquely sorted",
        ));
    }
    Ok(())
}

fn validate_sha256(value: &str, label: &str) -> Result<()> {
    if value.len() != 71
        || !value.starts_with("sha256:")
        || !value[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(WombatError::configuration(format!(
            "{label} is not a SHA-256 identity"
        )));
    }
    Ok(())
}

fn validate_source_trace(
    trace: &crate::model::manifest::SourceTrace,
    sources: &std::collections::BTreeSet<&str>,
    label: &str,
) -> Result<()> {
    if trace.callers.len() + 1 > crate::model::manifest::MAX_SOURCE_TRACE_FRAMES {
        return Err(WombatError::configuration(format!(
            "{label} exceeds the maximum source trace depth"
        )));
    }
    let mut previous = None;
    for location in std::iter::once(&trace.primary).chain(&trace.callers) {
        validate_relative_path(&location.source, &format!("{label} source"))?;
        if !sources.contains(location.source.as_str()) {
            return Err(WombatError::configuration(format!(
                "{label} references uncatalogued source `{}`",
                location.source
            )));
        }
        if location.line == Some(0) || location.column == Some(0) {
            return Err(WombatError::configuration(format!(
                "{label} contains a zero source position"
            )));
        }
        if location.column.is_some() && location.line.is_none() {
            return Err(WombatError::configuration(format!(
                "{label} contains a column without a line"
            )));
        }
        if previous == Some(location) {
            return Err(WombatError::configuration(format!(
                "{label} contains consecutive duplicate frames"
            )));
        }
        previous = Some(location);
    }
    Ok(())
}

fn verify_tree(tree: &Path, manifest: &Manifest) -> Result<()> {
    let mut expected_files = BTreeMap::new();
    let mut expected_dirs = BTreeSet::new();
    for artifact in &manifest.artifacts {
        let relative = artifact.target.path.clone();
        if expected_files.insert(relative.clone(), artifact).is_some() {
            return Err(WombatError::configuration(format!(
                "manifest contains duplicate tree path `{relative}`"
            )));
        }
        let mut parent = Path::new(&relative).parent();
        while let Some(path) = parent {
            if path.as_os_str().is_empty() {
                break;
            }
            expected_dirs.insert(path.to_string_lossy().replace('\\', "/"));
            parent = path.parent();
        }
    }
    let metadata = fs::symlink_metadata(tree).map_err(|error| WombatError::io(tree, error))?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(WombatError::configuration(format!(
            "build tree `{}` must be a non-symlink directory",
            tree.display()
        )));
    }
    let mut seen_files = BTreeSet::new();
    let mut seen_dirs = BTreeSet::new();
    walk_tree(
        tree,
        tree,
        &expected_files,
        &expected_dirs,
        &mut seen_files,
        &mut seen_dirs,
    )?;
    if seen_files.len() != expected_files.len() || seen_dirs != expected_dirs {
        return Err(WombatError::configuration(
            "build tree is missing manifest-required entries",
        ));
    }
    Ok(())
}

fn verify_provider_payloads(root: &Path, manifest: &Manifest) -> Result<()> {
    let providers_root = root.join("providers");
    let mut expected = BTreeMap::new();
    for provider in &manifest.providers {
        if let crate::model::manifest::ProviderOrigin::Custom { files, .. } = &provider.origin {
            for file in files {
                expected.insert(file.payload.as_str(), file);
            }
        }
    }
    if expected.is_empty() {
        if providers_root
            .try_exists()
            .map_err(|error| WombatError::io(&providers_root, error))?
        {
            return Err(WombatError::configuration(
                "build product contains an unexpected provider payload tree",
            ));
        }
        return Ok(());
    }
    ensure_plain_directory(&providers_root)?;
    let mut seen = BTreeSet::new();
    verify_provider_directory(&providers_root, &providers_root, &expected, &mut seen)?;
    if seen.len() != expected.len() {
        return Err(WombatError::configuration(
            "provider payload tree is missing manifest-required files",
        ));
    }
    Ok(())
}

fn verify_provider_directory<'a>(
    root: &Path,
    directory: &Path,
    expected: &BTreeMap<&'a str, &'a crate::model::manifest::ProviderFile>,
    seen: &mut BTreeSet<String>,
) -> Result<()> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| WombatError::io(directory, error))?
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|error| WombatError::io(directory, error))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .expect("provider payload remains under its root")
            .to_str()
            .ok_or_else(|| WombatError::configuration("provider payload path is not UTF-8"))?
            .replace('\\', "/");
        let metadata =
            fs::symlink_metadata(&path).map_err(|error| WombatError::io(&path, error))?;
        if metadata.file_type().is_symlink() {
            return Err(WombatError::configuration(format!(
                "provider payload `{relative}` must not be a symbolic link"
            )));
        }
        if metadata.file_type().is_dir() {
            let prefix = format!("{relative}/");
            if !expected
                .keys()
                .any(|candidate| candidate.starts_with(&prefix))
            {
                return Err(WombatError::configuration(format!(
                    "provider payload tree contains extra directory `{relative}`"
                )));
            }
            verify_provider_directory(root, &path, expected, seen)?;
        } else if metadata.file_type().is_file() {
            let file = expected.get(relative.as_str()).ok_or_else(|| {
                WombatError::configuration(format!(
                    "provider payload tree contains extra file `{relative}`"
                ))
            })?;
            let bytes = fs::read(&path).map_err(|error| WombatError::io(&path, error))?;
            if u64::try_from(bytes.len()).ok() != Some(file.size)
                || digest_string(Sha256::digest(&bytes)) != file.digest
                || executable_intent(&metadata)
            {
                return Err(WombatError::configuration(format!(
                    "provider payload `{relative}` does not match its manifest identity"
                )));
            }
            seen.insert(relative);
        } else {
            return Err(WombatError::configuration(format!(
                "provider payload `{relative}` must be a regular file or directory"
            )));
        }
    }
    Ok(())
}

fn walk_tree(
    root: &Path,
    directory: &Path,
    expected_files: &BTreeMap<String, &Artifact>,
    expected_dirs: &BTreeSet<String>,
    seen_files: &mut BTreeSet<String>,
    seen_dirs: &mut BTreeSet<String>,
) -> Result<()> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| WombatError::io(directory, error))?
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|error| WombatError::io(directory, error))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .expect("walked entries remain under root");
        let relative = relative
            .to_str()
            .ok_or_else(|| {
                WombatError::configuration(format!(
                    "build tree entry `{}` is not valid UTF-8",
                    path.display()
                ))
            })?
            .replace('\\', "/");
        let metadata =
            fs::symlink_metadata(&path).map_err(|error| WombatError::io(&path, error))?;
        if metadata.file_type().is_symlink() {
            return Err(WombatError::configuration(format!(
                "build tree entry `{relative}` must not be a symbolic link"
            )));
        }
        if metadata.file_type().is_dir() {
            if !expected_dirs.contains(&relative) {
                return Err(WombatError::configuration(format!(
                    "build tree contains extra directory `{relative}`"
                )));
            }
            seen_dirs.insert(relative);
            walk_tree(
                root,
                &path,
                expected_files,
                expected_dirs,
                seen_files,
                seen_dirs,
            )?;
        } else if metadata.file_type().is_file() {
            let artifact = expected_files.get(&relative).ok_or_else(|| {
                WombatError::configuration(format!("build tree contains extra file `{relative}`"))
            })?;
            verify_file(&path, artifact)?;
            seen_files.insert(relative);
        } else {
            return Err(WombatError::configuration(format!(
                "build tree entry `{relative}` is not a regular file or directory"
            )));
        }
    }
    Ok(())
}

fn verify_file(path: &Path, artifact: &Artifact) -> Result<()> {
    let mut file = File::open(path).map_err(|error| WombatError::io(path, error))?;
    let metadata = file
        .metadata()
        .map_err(|error| WombatError::io(path, error))?;
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| WombatError::io(path, error))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        size = size
            .checked_add(u64::try_from(count).expect("buffer lengths fit in u64"))
            .ok_or_else(|| WombatError::configuration("artifact size exceeds u64"))?;
    }
    let digest = digest_string(hasher.finalize());
    if size != artifact.content.size || digest != artifact.content.digest {
        return Err(WombatError::configuration(format!(
            "build tree file `{}` does not match its manifest content identity",
            path.display()
        )));
    }
    if executable_intent(&metadata) != artifact.content.executable {
        return Err(WombatError::configuration(format!(
            "build tree file `{}` has incorrect executable intent",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let expected = if artifact.content.executable {
            0o755
        } else {
            0o644
        };
        if metadata.permissions().mode() & 0o777 != expected {
            return Err(WombatError::configuration(format!(
                "build tree file `{}` has mode {:o}, expected {expected:o}",
                path.display(),
                metadata.permissions().mode() & 0o777
            )));
        }
    }
    Ok(())
}
