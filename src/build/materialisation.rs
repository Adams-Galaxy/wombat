//! Artifact materialisation, template rendering, and deterministic product identity.

use super::*;

pub(super) fn materialise_product(
    source_root: &Path,
    product_root: &Path,
    desired: crate::manifest::EvaluatedManifest,
    cache: &crate::cache::BuildCache,
    execution_mode: crate::manifest::ExecutionMode,
    skipped_requirement_gates: Vec<String>,
) -> Result<Manifest> {
    materialise_inner(
        source_root,
        product_root,
        desired,
        Some(cache),
        execution_mode,
        skipped_requirement_gates,
        |_| {},
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MaterialisationPoint {
    AfterArtifact(usize),
    BeforeFinalValidation,
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn materialise_with_hook(
    source_root: &Path,
    product_root: &Path,
    desired: crate::manifest::EvaluatedManifest,
    hook: impl FnMut(MaterialisationPoint),
) -> Result<Manifest> {
    materialise_inner(
        source_root,
        product_root,
        desired,
        None,
        crate::manifest::ExecutionMode::Normal,
        Vec::new(),
        hook,
    )
}

fn materialise_inner(
    source_root: &Path,
    product_root: &Path,
    desired: crate::manifest::EvaluatedManifest,
    cache: Option<&crate::cache::BuildCache>,
    execution_mode: crate::manifest::ExecutionMode,
    skipped_requirement_gates: Vec<String>,
    mut hook: impl FnMut(MaterialisationPoint),
) -> Result<Manifest> {
    let tree = product_root.join("tree");
    fs::create_dir(&tree).map_err(|error| WombatError::io(&tree, error))?;

    let mut artifacts = Vec::with_capacity(desired.artifacts.len());
    for (index, artifact) in desired.artifacts.iter().enumerate() {
        artifacts.push(materialise_artifact(source_root, &tree, artifact, cache)?);
        hook(MaterialisationPoint::AfterArtifact(index));
    }
    materialise_provider_payloads(source_root, product_root, &desired.providers)?;
    crate::execution::script::publish_payloads(
        source_root,
        product_root,
        &desired.scripts,
        crate::execution::script::PayloadKind::Product,
    )?;
    hook(MaterialisationPoint::BeforeFinalValidation);
    revalidate_sources(source_root, &desired.artifacts, &desired.directories)?;
    revalidate_lua_sources(source_root, &desired.sources)?;
    let mut manifest = Manifest {
        format_version: MANIFEST_FORMAT_VERSION,
        wombat_version: WOMBAT_VERSION.to_string(),
        build_id: String::new(),
        plan_id: desired.plan_id,
        execution_mode,
        skipped_requirement_gates,
        sources: desired.sources,
        inputs: desired.inputs,
        target: desired.target,
        observations: desired.observations,
        process_observations: desired.process_observations,
        modules: desired.modules,
        dependencies: desired.dependencies,
        project_identity: desired.project_identity,
        ladder: desired.ladder,
        providers: desired.providers,
        requirements: desired.requirements,
        preparations: desired.preparations,
        tasks: desired.tasks.into_iter().map(|task| task.task).collect(),
        scripts: desired.scripts,
        artifact_policy: desired.artifact_policy,
        artifact_notices: desired.artifact_notices,
        artifact_selections: desired.artifact_selections,
        artifacts,
    };
    manifest.build_id = compute_build_id(&manifest)?;
    write_manifest(&product_root.join("manifest.json"), &manifest)?;
    Ok(manifest)
}

fn materialise_provider_payloads(
    source_root: &Path,
    product_root: &Path,
    providers: &[crate::manifest::Provider],
) -> Result<()> {
    let custom = providers
        .iter()
        .filter_map(|provider| match &provider.origin {
            crate::manifest::ProviderOrigin::Custom { files, .. } => Some(files.as_slice()),
            crate::manifest::ProviderOrigin::Builtin { .. } => None,
        })
        .flatten()
        .collect::<Vec<_>>();
    if custom.is_empty() {
        return Ok(());
    }
    let payload_root = product_root.join("providers");
    fs::create_dir(&payload_root).map_err(|error| WombatError::io(&payload_root, error))?;
    for file in custom {
        let source = source_root.join(&file.source);
        reject_source_symlinks(source_root, &source)?;
        let bytes = fs::read(&source).map_err(|error| WombatError::io(&source, error))?;
        if digest_string(Sha256::digest(&bytes)) != file.digest
            || u64::try_from(bytes.len()).ok() != Some(file.size)
        {
            return Err(WombatError::configuration(format!(
                "provider source `{}` changed during materialisation",
                file.source
            )));
        }
        let destination = payload_root.join(&file.payload);
        let parent = destination
            .parent()
            .expect("provider payload files have a parent");
        fs::create_dir_all(parent).map_err(|error| WombatError::io(parent, error))?;
        write_bytes(&destination, &bytes)?;
        set_normalized_permissions(
            &OpenOptions::new()
                .read(true)
                .write(true)
                .open(&destination)
                .map_err(|error| WombatError::io(&destination, error))?,
            false,
            &destination,
        )?;
    }
    Ok(())
}

fn revalidate_lua_sources(
    source_root: &Path,
    sources: &[crate::manifest::SourceFile],
) -> Result<()> {
    for source in sources {
        let path = source_root.join(source.path.replace('/', std::path::MAIN_SEPARATOR_STR));
        reject_source_symlinks(source_root, &path)?;
        let metadata =
            fs::symlink_metadata(&path).map_err(|error| WombatError::io(&path, error))?;
        if !metadata.file_type().is_file() {
            return Err(WombatError::configuration(format!(
                "Lua source `{}` is no longer a regular file",
                source.path
            )));
        }
        let bytes = fs::read(&path).map_err(|error| WombatError::io(&path, error))?;
        let digest = digest_string(Sha256::digest(&bytes));
        if digest != source.digest {
            return Err(WombatError::configuration(format!(
                "Lua source `{}` changed during materialisation",
                source.path
            )));
        }
    }
    Ok(())
}

fn materialise_artifact(
    source_root: &Path,
    tree: &Path,
    artifact: &EvaluatedArtifact,
    cache: Option<&crate::cache::BuildCache>,
) -> Result<Artifact> {
    let source_path = source_root.join(&artifact.source);
    let destination = tree.join(&artifact.target.path);
    let parent = destination.parent().expect("file artifacts have a parent");
    fs::create_dir_all(parent).map_err(|error| WombatError::io(parent, error))?;
    let (production, content) = match &artifact.production {
        EvaluatedProduction::Static => {
            reject_source_symlinks(source_root, &source_path)?;
            (
                Production::Static,
                copy_and_hash(
                    &source_path,
                    &destination,
                    artifact
                        .fingerprint
                        .as_ref()
                        .expect("static artifacts have fingerprints"),
                )?,
            )
        }
        EvaluatedProduction::Template { context } => {
            reject_source_symlinks(source_root, &source_path)?;
            let (source_digest, content) = render_and_hash(
                &source_path,
                &artifact.source,
                &destination,
                artifact
                    .fingerprint
                    .as_ref()
                    .expect("template artifacts have fingerprints"),
                context,
                cache,
            )?;
            (
                Production::Template {
                    renderer: RendererIdentity {
                        name: TEMPLATE_RENDERER_NAME.to_string(),
                        contract_version: TEMPLATE_CONTRACT_VERSION,
                    },
                    source_digest,
                    context: context.clone(),
                },
                content,
            )
        }
        EvaluatedProduction::GeneratedLua {
            content,
            executable,
        } => (
            Production::GeneratedLua {
                contract_version: 1,
            },
            write_generated(&destination, content, *executable)?,
        ),
        EvaluatedProduction::Task {
            identity,
            output,
            content,
            executable,
        } => (
            Production::Task {
                contract_version: 1,
                identity: identity.clone(),
                output: output.clone(),
            },
            write_generated(&destination, content, *executable)?,
        ),
    };
    Ok(Artifact {
        kind: artifact.kind,
        source: artifact.source.clone(),
        source_origin: artifact.source_origin.clone(),
        source_projection: artifact.source_projection.clone(),
        production,
        target: artifact.target.clone(),
        content,
        owner: artifact.owner.clone(),
        declared_at: artifact.declared_at.clone(),
    })
}

fn write_generated(destination: &Path, bytes: &[u8], executable: bool) -> Result<FileContent> {
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)
        .map_err(|error| WombatError::io(destination, error))?;
    output
        .write_all(bytes)
        .map_err(|error| WombatError::io(destination, error))?;
    set_normalized_permissions(&output, executable, destination)?;
    output
        .sync_all()
        .map_err(|error| WombatError::io(destination, error))?;
    Ok(FileContent {
        digest: digest_string(Sha256::digest(bytes)),
        size: u64::try_from(bytes.len())
            .map_err(|_| WombatError::configuration("generated artifact exceeds u64"))?,
        executable,
    })
}

fn render_and_hash(
    source: &Path,
    source_name: &str,
    destination: &Path,
    expected: &SourceFingerprint,
    context: &crate::frozen::FrozenValue,
    cache: Option<&crate::cache::BuildCache>,
) -> Result<(String, FileContent)> {
    let mut input = File::open(source).map_err(|error| WombatError::io(source, error))?;
    let before = input
        .metadata()
        .map_err(|error| WombatError::io(source, error))?;
    if !before.file_type().is_file() || SourceFingerprint::from_metadata(&before) != *expected {
        return Err(source_changed(source));
    }
    let mut bytes = Vec::new();
    input
        .read_to_end(&mut bytes)
        .map_err(|error| WombatError::io(source, error))?;
    let after = input
        .metadata()
        .map_err(|error| WombatError::io(source, error))?;
    let path_after =
        fs::symlink_metadata(source).map_err(|error| WombatError::io(source, error))?;
    if SourceFingerprint::from_metadata(&after) != *expected
        || SourceFingerprint::from_metadata(&path_after) != *expected
    {
        return Err(source_changed(source));
    }
    let template_source = std::str::from_utf8(&bytes).map_err(|error| {
        WombatError::configuration(format!(
            "template source `{source_name}` is not valid UTF-8: {error}"
        ))
    })?;
    let source_digest = digest_string(Sha256::digest(&bytes));

    #[derive(Serialize)]
    struct TemplateKey<'a> {
        renderer: &'a str,
        contract_version: u32,
        source_digest: &'a str,
        context: &'a crate::frozen::FrozenValue,
    }
    let cache_key = cache
        .map(|cache| {
            cache.key(
                "template-v1",
                &TemplateKey {
                    renderer: TEMPLATE_RENDERER_NAME,
                    contract_version: TEMPLATE_CONTRACT_VERSION,
                    source_digest: &source_digest,
                    context,
                },
            )
        })
        .transpose()?;
    let executable = executable_intent(&before);
    if let (Some(cache), Some(key)) = (cache, cache_key.as_deref())
        && let Some(rendered) = cache.load_template(key)?
    {
        let content = write_generated(destination, &rendered, executable)?;
        return Ok((source_digest, content));
    }

    let mut renderer = handlebars::Handlebars::new();
    renderer.set_strict_mode(true);
    renderer.set_recursive_lookup(false);
    renderer.register_escape_fn(handlebars::no_escape);
    for helper in [
        "lookup", "log", "eq", "ne", "gt", "gte", "lt", "lte", "and", "or", "not", "len",
    ] {
        renderer.unregister_helper(helper);
    }
    renderer.register_helper("if", Box::new(StrictConditionalHelper::new("if", true)));
    renderer.register_helper(
        "unless",
        Box::new(StrictConditionalHelper::new("unless", false)),
    );
    let template = handlebars::Template::compile(template_source)
        .map_err(|error| template_compile_error(source_name, template_source, error))?;
    validate_handlebars_contract(source_name, &template)?;
    renderer.register_template(source_name, template);
    let rendered = renderer
        .render(source_name, context)
        .map_err(|error| template_render_error(source_name, template_source, error))?;
    let rendered = rendered.as_bytes();

    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)
        .map_err(|error| WombatError::io(destination, error))?;
    output
        .write_all(rendered)
        .map_err(|error| WombatError::io(destination, error))?;
    set_normalized_permissions(&output, executable, destination)?;
    output
        .sync_all()
        .map_err(|error| WombatError::io(destination, error))?;
    if let (Some(cache), Some(key)) = (cache, cache_key.as_deref()) {
        cache.store_template(key, rendered)?;
    }
    Ok((
        source_digest,
        FileContent {
            digest: digest_string(Sha256::digest(rendered)),
            size: u64::try_from(rendered.len())
                .map_err(|_| WombatError::configuration("artifact size exceeds u64"))?,
            executable,
        },
    ))
}

fn template_compile_error(
    source_name: &str,
    source: &str,
    error: handlebars::TemplateError,
) -> WombatError {
    let position = error.pos();
    template_diagnostic(
        format!(
            "failed to compile template `{source_name}`: {}",
            error.reason()
        ),
        source_name,
        source,
        position,
        error.to_string(),
    )
}

fn template_render_error(
    source_name: &str,
    source: &str,
    error: handlebars::RenderError,
) -> WombatError {
    let position = error.line_no.zip(error.column_no);
    template_diagnostic(
        format!("failed to render template `{source_name}`: {error}"),
        source_name,
        source,
        position,
        error.to_string(),
    )
}

fn template_diagnostic(
    message: String,
    source_name: &str,
    source: &str,
    position: Option<(usize, usize)>,
    underlying: String,
) -> WombatError {
    let line = position.and_then(|(line, _)| u32::try_from(line).ok());
    let column = position.and_then(|(_, column)| u32::try_from(column).ok());
    let mut diagnostic = crate::Diagnostic::new(message);
    diagnostic.primary = Some(crate::manifest::SourceLocation {
        source: source_name.to_string(),
        line,
        column,
    });
    diagnostic.source_line = line.and_then(|line| {
        source
            .lines()
            .nth(line.saturating_sub(1) as usize)
            .map(str::to_string)
    });
    diagnostic.underlying = Some(underlying);
    WombatError::diagnostic(diagnostic)
}

struct StrictConditionalHelper {
    name: &'static str,
    positive: bool,
}

impl StrictConditionalHelper {
    fn new(name: &'static str, positive: bool) -> Self {
        Self { name, positive }
    }
}

impl handlebars::HelperDef for StrictConditionalHelper {
    fn call<'reg: 'rc, 'rc>(
        &self,
        helper: &handlebars::Helper<'rc>,
        renderer: &'reg handlebars::Handlebars<'reg>,
        context: &'rc handlebars::Context,
        render_context: &mut handlebars::RenderContext<'reg, 'rc>,
        output: &mut dyn handlebars::Output,
    ) -> handlebars::HelperResult {
        let value = helper
            .param(0)
            .ok_or(handlebars::RenderErrorReason::ParamNotFoundForIndex(
                self.name, 0,
            ))?;
        if value.is_value_missing() {
            return Err(handlebars::RenderError::strict_error(value.relative_path()));
        }
        let truthy = handlebars_truthy(value.value());
        let template = if truthy == self.positive {
            helper.template()
        } else {
            helper.inverse()
        };
        template.map_or(Ok(()), |template| {
            handlebars::Renderable::render(template, renderer, context, render_context, output)
        })
    }
}

fn handlebars_truthy(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(value) => *value,
        serde_json::Value::Number(value) => value.as_f64().is_some_and(f64::is_normal),
        serde_json::Value::String(value) => !value.is_empty(),
        serde_json::Value::Array(value) => !value.is_empty(),
        serde_json::Value::Object(value) => !value.is_empty(),
    }
}

fn validate_handlebars_contract(source_name: &str, template: &handlebars::Template) -> Result<()> {
    use handlebars::template::TemplateElement;

    for (index, element) in template.elements.iter().enumerate() {
        let location = template
            .mapping
            .get(index)
            .map_or(String::new(), |mapping| {
                format!(" at line {}, column {}", mapping.0, mapping.1)
            });
        match element {
            TemplateElement::RawString(_) | TemplateElement::Comment(_) => {}
            TemplateElement::Expression(helper) | TemplateElement::HtmlExpression(helper) => {
                if !helper.params.is_empty() || !helper.hash.is_empty() {
                    return Err(unsupported_handlebars_feature(
                        source_name,
                        &location,
                        "inline helpers",
                    ));
                }
            }
            TemplateElement::HelperBlock(helper) => {
                let name = helper.name.as_name().unwrap_or("<dynamic>");
                if !matches!(name, "if" | "unless" | "each" | "with" | "raw") {
                    return Err(unsupported_handlebars_feature(
                        source_name,
                        &location,
                        &format!("helper `{name}`"),
                    ));
                }
                if !helper.hash.is_empty()
                    || helper
                        .params
                        .iter()
                        .any(handlebars_parameter_has_subexpression)
                {
                    return Err(unsupported_handlebars_feature(
                        source_name,
                        &location,
                        "helper hash arguments and subexpressions",
                    ));
                }
                if matches!(name, "each" | "with") && helper.inverse.is_some() {
                    return Err(unsupported_handlebars_feature(
                        source_name,
                        &location,
                        "else blocks on `each` or `with`",
                    ));
                }
                if let Some(body) = &helper.template {
                    validate_handlebars_contract(source_name, body)?;
                }
                if let Some(inverse) = &helper.inverse {
                    validate_handlebars_contract(source_name, inverse)?;
                }
            }
            TemplateElement::DecoratorExpression(_)
            | TemplateElement::DecoratorBlock(_)
            | TemplateElement::PartialExpression(_)
            | TemplateElement::PartialBlock(_) => {
                return Err(unsupported_handlebars_feature(
                    source_name,
                    &location,
                    "decorators and partials",
                ));
            }
            _ => {
                return Err(unsupported_handlebars_feature(
                    source_name,
                    &location,
                    "this template construct",
                ));
            }
        }
    }
    Ok(())
}

fn handlebars_parameter_has_subexpression(parameter: &handlebars::template::Parameter) -> bool {
    matches!(parameter, handlebars::template::Parameter::Subexpression(_))
}

fn unsupported_handlebars_feature(source_name: &str, location: &str, feature: &str) -> WombatError {
    WombatError::configuration(format!(
        "template `{source_name}` uses unsupported Handlebars {feature}{location}; resolve policy and transformations in Lua"
    ))
}

fn copy_and_hash(
    source: &Path,
    destination: &Path,
    expected: &SourceFingerprint,
) -> Result<FileContent> {
    copy_and_hash_with_hook(source, destination, expected, || {})
}

pub(super) fn copy_and_hash_with_hook(
    source: &Path,
    destination: &Path,
    expected: &SourceFingerprint,
    after_copy: impl FnOnce(),
) -> Result<FileContent> {
    let mut input = File::open(source).map_err(|error| WombatError::io(source, error))?;
    let before = input
        .metadata()
        .map_err(|error| WombatError::io(source, error))?;
    if !before.file_type().is_file() {
        return Err(WombatError::configuration(format!(
            "static artifact source `{}` is not a regular file",
            source.display()
        )));
    }
    if SourceFingerprint::from_metadata(&before) != *expected {
        return Err(source_changed(source));
    }
    let executable = executable_intent(&before);
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)
        .map_err(|error| WombatError::io(destination, error))?;
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = input
            .read(&mut buffer)
            .map_err(|error| WombatError::io(source, error))?;
        if count == 0 {
            break;
        }
        output
            .write_all(&buffer[..count])
            .map_err(|error| WombatError::io(destination, error))?;
        hasher.update(&buffer[..count]);
        size = size
            .checked_add(u64::try_from(count).expect("buffer lengths fit in u64"))
            .ok_or_else(|| WombatError::configuration("artifact size exceeds u64"))?;
    }
    after_copy();
    let after = input
        .metadata()
        .map_err(|error| WombatError::io(source, error))?;
    let path_after =
        fs::symlink_metadata(source).map_err(|error| WombatError::io(source, error))?;
    if SourceFingerprint::from_metadata(&after) != *expected
        || SourceFingerprint::from_metadata(&path_after) != *expected
    {
        return Err(source_changed(source));
    }
    set_normalized_permissions(&output, executable, destination)?;
    output
        .sync_all()
        .map_err(|error| WombatError::io(destination, error))?;
    Ok(FileContent {
        digest: digest_string(hasher.finalize()),
        size,
        executable,
    })
}

fn source_changed(source: &Path) -> WombatError {
    WombatError::configuration(format!(
        "artifact source `{}` changed during materialisation",
        source.display()
    ))
}

pub(super) fn revalidate_sources(
    source_root: &Path,
    artifacts: &[EvaluatedArtifact],
    directories: &[EvaluatedDirectory],
) -> Result<()> {
    for artifact in artifacts {
        let Some(expected) = &artifact.fingerprint else {
            continue;
        };
        let source = source_root.join(&artifact.source);
        if fingerprint_regular_file(&source)? != *expected {
            return Err(source_changed(&source));
        }
    }
    for directory in directories {
        let source = source_root.join(&directory.root);
        let exclusion_matchers = directory
            .exclusions
            .iter()
            .map(|value| {
                crate::selection::compile_selector(value, directory.hidden)
                    .and_then(|selector| crate::selection::matcher(&selector.physical))
            })
            .collect::<Result<Vec<_>>>()?;
        let snapshot =
            snapshot_directory_filtered(source_root, &source, |relative, is_directory| {
                let visible = if directory.glob {
                    crate::selection::in_static_scope(relative, &directory.static_root)
                        && crate::selection::hidden_components_authorized(
                            relative,
                            &directory.physical_selector,
                        )
                } else {
                    !relative
                        .split('/')
                        .any(crate::selection::is_hidden_component)
                };
                visible
                    && !crate::selection::is_excluded(&exclusion_matchers, relative, is_directory)
            })?;
        if snapshot != directory.snapshot {
            return Err(WombatError::configuration(format!(
                "static directory source `{}` changed during materialisation",
                source.display()
            )));
        }
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn executable_intent(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
pub(super) fn executable_intent(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn set_normalized_permissions(file: &File, executable: bool, path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(if executable {
        0o755
    } else {
        0o644
    }))
    .map_err(|error| WombatError::io(path, error))
}

#[cfg(not(unix))]
fn set_normalized_permissions(_file: &File, _executable: bool, _path: &Path) -> Result<()> {
    Ok(())
}

fn reject_source_symlinks(root: &Path, source: &Path) -> Result<()> {
    let relative = source.strip_prefix(root).map_err(|_| {
        WombatError::configuration(format!(
            "static artifact source `{}` escapes the repository",
            source.display()
        ))
    })?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        let metadata =
            fs::symlink_metadata(&current).map_err(|error| WombatError::io(&current, error))?;
        if metadata.file_type().is_symlink() {
            return Err(WombatError::configuration(format!(
                "static artifact source `{}` must not contain symbolic links",
                source.display()
            )));
        }
    }
    Ok(())
}

pub(super) fn compute_build_id(manifest: &Manifest) -> Result<String> {
    let payload = IdentityPayload {
        format_version: manifest.format_version,
        wombat_version: &manifest.wombat_version,
        plan_id: &manifest.plan_id,
        sources: &manifest.sources,
        inputs: &manifest.inputs,
        target: &manifest.target,
        observations: &manifest.observations,
        process_observations: &manifest.process_observations,
        modules: &manifest.modules,
        dependencies: &manifest.dependencies,
        project_identity: &manifest.project_identity,
        ladder: &manifest.ladder,
        providers: &manifest.providers,
        requirements: &manifest.requirements,
        preparations: &manifest.preparations,
        tasks: &manifest.tasks,
        scripts: &manifest.scripts,
        artifact_policy: &manifest.artifact_policy,
        artifact_notices: &manifest.artifact_notices,
        artifact_selections: &manifest.artifact_selections,
        artifacts: &manifest.artifacts,
    };
    let bytes = serde_json::to_vec(&payload)?;
    Ok(digest_string(Sha256::digest(bytes)))
}

fn write_manifest(path: &Path, manifest: &Manifest) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(manifest)?;
    bytes.push(b'\n');
    write_bytes(path, &bytes)
}

pub(super) fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    let parent = path.parent().expect("workspace files have parents");
    let mut temporary =
        NamedTempFile::new_in(parent).map_err(|error| WombatError::io(parent, error))?;
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

fn write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(|error| WombatError::io(path, error))?;
    file.write_all(bytes)
        .map_err(|error| WombatError::io(path, error))?;
    file.sync_all()
        .map_err(|error| WombatError::io(path, error))
}
