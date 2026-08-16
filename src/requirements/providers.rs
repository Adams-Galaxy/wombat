//! Built-in and Lua provider execution for checked prerequisites, shared
//! preparation, and requirement reconciliation.

use super::check::check_brew;
use super::*;

pub(super) fn check_custom(
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

pub(super) fn check_custom_prerequisite(
    context: &RequirementContext<'_>,
    provider: &Provider,
    prerequisite: &ProviderPrerequisite,
) -> Result<(CheckStatus, String)> {
    let runtime = CustomRuntime::load(context, provider, false, false, false)?;
    let check: Function = runtime
        .definition
        .get("check_prerequisite")
        .map_err(|error| {
            WombatError::configuration(format!(
                "provider `{}` requires check_prerequisite(): {error}",
                provider.name
            ))
        })?;
    let value: Value = check
        .call((
            runtime.context,
            frozen_prerequisite(prerequisite)?.to_lua(&runtime.lua)?,
        ))
        .map_err(|error| {
            WombatError::configuration(format!(
                "provider `{}` check_prerequisite failed: {error}",
                provider.name
            ))
        })?;
    parse_status(
        FrozenValue::from_lua(value)?,
        "provider check_prerequisite()",
    )
}

pub(super) fn prepare_provider(
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

pub(super) fn reconcile_prerequisite(
    context: &RequirementContext<'_>,
    prerequisite: &ProviderPrerequisite,
    observation: &CheckItem,
    noninteractive: bool,
) -> Result<()> {
    let provider = provider_for(context.providers, &prerequisite.provider)?;
    match &provider.origin {
        ProviderOrigin::Builtin { .. } if provider.name == "apt" => {
            reconcile_apt_source(context, prerequisite, noninteractive)
        }
        ProviderOrigin::Builtin { .. } => Err(WombatError::configuration(format!(
            "built-in provider `{}` does not support prerequisites",
            provider.name
        ))),
        ProviderOrigin::Custom { .. } => {
            let runtime = CustomRuntime::load(
                context,
                provider,
                true,
                prerequisite.elevated,
                noninteractive,
            )?;
            let reconcile: Function =
                runtime
                    .definition
                    .get("reconcile_prerequisite")
                    .map_err(|error| {
                        WombatError::configuration(format!(
                            "provider `{}` requires reconcile_prerequisite(): {error}",
                            provider.name
                        ))
                    })?;
            reconcile
                .call::<Value>((
                    runtime.context,
                    frozen_prerequisite(prerequisite)?.to_lua(&runtime.lua)?,
                    FrozenValue::Map(BTreeMap::from([
                        (
                            "status".to_string(),
                            FrozenValue::String(observation.status.as_str().to_string()),
                        ),
                        (
                            "detail".to_string(),
                            FrozenValue::String(observation.detail.clone()),
                        ),
                    ]))
                    .to_lua(&runtime.lua)?,
                ))
                .map_err(|error| {
                    WombatError::configuration(format!(
                        "provider `{}` reconcile_prerequisite failed: {error}",
                        provider.name
                    ))
                })?;
            Ok(())
        }
    }
}

pub(super) fn preflight(
    context: &RequirementContext<'_>,
    preparations: &[&ProviderPreparation],
    pending: &[&CheckItem],
) -> Result<()> {
    for item in pending
        .iter()
        .filter(|item| item.subject == CheckSubject::Prerequisite)
    {
        let prerequisite = prerequisite_for_item(context, item)?;
        let provider = provider_for(context.providers, &prerequisite.provider)?;
        match &provider.origin {
            ProviderOrigin::Builtin { .. } if provider.name == "apt" => {
                let source = apt_source(prerequisite)?;
                if apt_source_needs_download(context, &source)? {
                    require_command("curl", "Apt source key download")?;
                }
                for command in ["install", "mv", "rm"] {
                    require_command(command, "Apt source publication")?;
                }
                preflight_elevation(prerequisite.elevated)?;
            }
            ProviderOrigin::Builtin { .. } => {
                return Err(WombatError::configuration(format!(
                    "built-in provider `{}` does not support prerequisites",
                    provider.name
                )));
            }
            ProviderOrigin::Custom { .. } => {
                let runtime = CustomRuntime::load(context, provider, false, false, false)?;
                runtime
                    .definition
                    .get::<Function>("reconcile_prerequisite")
                    .map_err(|error| {
                        WombatError::configuration(format!(
                            "provider `{}` requires reconcile_prerequisite(): {error}",
                            provider.name
                        ))
                    })?;
                preflight_elevation(prerequisite.elevated)?;
            }
        }
    }
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
    for item in pending
        .iter()
        .filter(|item| item.subject == CheckSubject::Requirement)
    {
        let requirement = requirement_for_item(context, item)?;
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
                if !output.success {
                    return Err(WombatError::configuration(format!(
                        "Homebrew preflight failed for `{name}`: {}",
                        output_detail(&output)
                    )));
                }
            }
            ProviderOrigin::Builtin { .. } if provider.name == "apt" => {
                if !requirement.binding.prerequisites.is_empty() {
                    preflight_elevation(true)?;
                    continue;
                }
                let name = apt_identity(&requirement.binding)?;
                let apt_get = require_command("apt-get", "Apt preflight")?;
                let output = run_bounded(
                    &apt_get,
                    &["--simulate", "install", name],
                    &apt_environment(),
                )?;
                if !output.success {
                    return Err(WombatError::configuration(format!(
                        "Apt preflight failed for `{name}`: {}",
                        output_detail(&output)
                    )));
                }
                preflight_elevation(true)?;
            }
            ProviderOrigin::Builtin { .. } if provider.name == "git" => {
                let (repository, to, _reference) = git_identity(&requirement.binding)?;
                let git = require_command("git", "Git preflight")?;
                if !confirm_or_absent_git_checkout(&git, to, repository)? {
                    let probe = run_bounded(
                        &git,
                        &["ls-remote", "--exit-code", repository],
                        &BTreeMap::new(),
                    )?;
                    if !probe.success {
                        return Err(WombatError::configuration(format!(
                            "Git preflight failed for `{repository}`: {}",
                            output_detail(&probe)
                        )));
                    }
                }
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

pub(super) fn preflight_apt_requirement(
    _context: &RequirementContext<'_>,
    requirement: &Requirement,
) -> Result<()> {
    let name = apt_identity(&requirement.binding)?;
    let apt_get = require_command("apt-get", "Apt preflight")?;
    let output = run_bounded(
        &apt_get,
        &["--simulate", "install", name],
        &apt_environment(),
    )?;
    if !output.success {
        return Err(WombatError::configuration(format!(
            "Apt preflight failed for `{name}`: {}",
            output_detail(&output)
        )));
    }
    preflight_elevation(true)
}

pub(super) fn reconcile_requirement(
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
            let mut command = Command::new(&brew);
            command
                .args([operation, brew_flag(kind), name])
                .envs(brew_environment());
            let child_status = crate::execution::process::run_inherited(&mut command, "Homebrew")?;
            if !child_status.success {
                return Err(WombatError::configuration(format!(
                    "Homebrew {operation} failed for `{name}` with {}",
                    child_status.status
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
        ProviderOrigin::Builtin { .. } if provider.name == "git" => {
            let (repository, to, reference) = git_identity(&requirement.binding)?;
            let git = require_command("git", "Git bootstrap")?;
            if !confirm_or_absent_git_checkout(&git, to, repository)? {
                if let Some(parent) = Path::new(to).parent() {
                    fs::create_dir_all(parent).map_err(|error| WombatError::io(parent, error))?;
                }
                run_mutating(
                    &git,
                    &["clone", "--", repository, to],
                    &BTreeMap::new(),
                    false,
                    noninteractive,
                )?;
            }
            if let Some(reference) = reference {
                run_mutating(
                    &git,
                    &["-C", to, "fetch", "--tags", "--", "origin"],
                    &BTreeMap::new(),
                    false,
                    noninteractive,
                )?;
                run_mutating(
                    &git,
                    &["-C", to, "checkout", reference, "--"],
                    &BTreeMap::new(),
                    false,
                    noninteractive,
                )?;
            }
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

pub(super) fn runtime_provider_api(lua: &Lua) -> Result<Table> {
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
        "prerequisite",
        lua.create_function(|_, table: Table| {
            table.set("kind", "prerequisite")?;
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

pub(super) fn install_product_require(
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

pub(super) fn process_context(
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
                table.set("success", status.success)?;
                table.set("code", status.code)?;
                Ok(table)
            })?,
        )?;
    }
    Ok(context)
}

pub(super) fn process_spec(spec: Table) -> mlua::Result<ProcessSpec> {
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

pub(super) fn output_table(lua: &Lua, output: &ProcessOutcome) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("success", output.success)?;
    table.set("code", output.code)?;
    table.set(
        "stdout",
        String::from_utf8_lossy(&output.stdout.bytes).into_owned(),
    )?;
    table.set(
        "stderr",
        String::from_utf8_lossy(&output.stderr.bytes).into_owned(),
    )?;
    Ok(table)
}

pub(super) fn parse_custom_status(
    binding: &ProviderBinding,
    value: FrozenValue,
) -> Result<CheckItem> {
    let (status, detail) = parse_status(value, "provider check()")?;
    Ok(provider_item(binding, status, &detail))
}

fn parse_status(value: FrozenValue, operation: &str) -> Result<(CheckStatus, String)> {
    let FrozenValue::Map(mut values) = value else {
        return Err(WombatError::configuration(format!(
            "{operation} must return a status table"
        )));
    };
    let status = match values.remove("status") {
        Some(FrozenValue::String(value)) if value == "satisfied" => CheckStatus::Satisfied,
        Some(FrozenValue::String(value)) if value == "missing" => CheckStatus::Missing,
        Some(FrozenValue::String(value)) if value == "outdated" => CheckStatus::Outdated,
        Some(FrozenValue::String(value)) if value == "unavailable" => CheckStatus::Unavailable,
        _ => {
            return Err(WombatError::configuration(format!(
                "{operation} returned an invalid status"
            )));
        }
    };
    let detail = match values.remove("detail") {
        None => status.as_str().to_string(),
        Some(FrozenValue::String(value)) => value,
        Some(value) => serde_json::to_string(&value)?,
    };
    if !values.is_empty() {
        return Err(WombatError::configuration(format!(
            "{operation} returned unknown status fields"
        )));
    }
    Ok((status, detail))
}

pub(super) fn ensure_compatible_host(manifest: &Manifest) -> Result<()> {
    ensure_compatible_platform(&manifest.target.platform)
}

pub(super) fn ensure_compatible_platform(
    platform: &crate::model::context::TargetPlatform,
) -> Result<()> {
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

pub(super) fn selected_candidate(requirement: &Requirement) -> Result<&RequirementCandidate> {
    requirement
        .candidates
        .get(requirement.selected as usize)
        .ok_or_else(|| {
            WombatError::configuration("requirement selection is outside its candidates")
        })
}

pub(super) fn requirement_label(requirement: &Requirement) -> String {
    format!(
        "{}:{}",
        match requirement.kind {
            RequirementKind::Command => "command",
            RequirementKind::Package => "package",
        },
        requirement.candidates[requirement.selected as usize].name()
    )
}

pub(super) fn provider_for<'a>(providers: &'a [Provider], name: &str) -> Result<&'a Provider> {
    providers
        .iter()
        .find(|provider| provider.name == name)
        .ok_or_else(|| WombatError::configuration(format!("selected provider `{name}` is absent")))
}

pub(super) fn requirement_for_item<'a>(
    context: &'a RequirementContext<'_>,
    item: &CheckItem,
) -> Result<&'a Requirement> {
    context
        .requirements
        .iter()
        .find(|requirement| requirement_label(requirement) == item.identity)
        .ok_or_else(|| WombatError::configuration("check item references an absent requirement"))
}

pub(super) fn prerequisite_for_item<'a>(
    context: &'a RequirementContext<'_>,
    item: &CheckItem,
) -> Result<&'a ProviderPrerequisite> {
    context
        .prerequisites
        .iter()
        .find(|prerequisite| {
            item.identity
                == format!(
                    "prerequisite:{}:{}",
                    prerequisite.provider, prerequisite.identity
                )
        })
        .ok_or_else(|| WombatError::configuration("check item references an absent prerequisite"))
}

pub(super) fn provider_item(
    binding: &ProviderBinding,
    status: CheckStatus,
    detail: &str,
) -> CheckItem {
    CheckItem {
        subject: CheckSubject::Requirement,
        identity: String::new(),
        provider: binding.provider.clone(),
        status,
        detail: detail.to_string(),
        duration_ms: 0,
    }
}

pub(super) fn frozen_binding(binding: &ProviderBinding) -> Result<FrozenValue> {
    Ok(serde_json::from_value(serde_json::to_value(binding)?)?)
}

pub(super) fn frozen_preparation(preparation: &ProviderPreparation) -> Result<FrozenValue> {
    Ok(serde_json::from_value(serde_json::to_value(preparation)?)?)
}

pub(super) fn frozen_prerequisite(prerequisite: &ProviderPrerequisite) -> Result<FrozenValue> {
    Ok(serde_json::from_value(serde_json::to_value(prerequisite)?)?)
}

pub(super) fn apt_identity(binding: &ProviderBinding) -> Result<&str> {
    let FrozenValue::Map(data) = &binding.data else {
        return Err(WombatError::configuration("Apt binding data must be a map"));
    };
    match data.get("name") {
        Some(FrozenValue::String(value)) => Ok(value),
        _ => Err(WombatError::configuration("Apt binding lacks package name")),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct AptSource {
    pub name: String,
    pub uri: String,
    pub suite: String,
    pub components: Vec<String>,
    pub architectures: Vec<String>,
    pub key_url: String,
    pub key_format: String,
    pub key_sha256: Option<String>,
    pub replace: bool,
}

impl AptSource {
    pub(super) fn source_relative_path(&self) -> PathBuf {
        PathBuf::from(format!(
            "etc/apt/sources.list.d/wombat-{}.sources",
            self.name
        ))
    }

    pub(super) fn key_relative_path(&self) -> PathBuf {
        PathBuf::from(format!(
            "etc/apt/keyrings/wombat-{}.{}",
            self.name, self.key_format
        ))
    }

    pub(super) fn marker(&self) -> String {
        format!("# Managed by Wombat: apt-source:{}\n", self.name)
    }

    pub(super) fn deb822(&self) -> String {
        let mut value = format!(
            "{}Types: deb\nURIs: {}\nSuites: {}\nComponents: {}\n",
            self.marker(),
            self.uri,
            self.suite,
            self.components.join(" ")
        );
        if !self.architectures.is_empty() {
            value.push_str(&format!(
                "Architectures: {}\n",
                self.architectures.join(" ")
            ));
        }
        value.push_str(&format!(
            "Signed-By: /etc/apt/keyrings/wombat-{}.{}\n",
            self.name, self.key_format
        ));
        value
    }
}

pub(super) fn apt_source(prerequisite: &ProviderPrerequisite) -> Result<AptSource> {
    let FrozenValue::Map(data) = &prerequisite.data else {
        return Err(WombatError::configuration(
            "Apt source prerequisite data must be a map",
        ));
    };
    let string = |name: &str| match data.get(name) {
        Some(FrozenValue::String(value)) => Ok(value.clone()),
        _ => Err(WombatError::configuration(format!(
            "Apt source prerequisite lacks `{name}`"
        ))),
    };
    let strings = |name: &str, required: bool| match data.get(name) {
        Some(FrozenValue::Array(values)) => values
            .iter()
            .map(|value| match value {
                FrozenValue::String(value) => Ok(value.clone()),
                _ => Err(WombatError::configuration(format!(
                    "Apt source prerequisite `{name}` must contain strings"
                ))),
            })
            .collect(),
        None if !required => Ok(Vec::new()),
        _ => Err(WombatError::configuration(format!(
            "Apt source prerequisite lacks `{name}` array"
        ))),
    };
    let FrozenValue::Map(key) = data
        .get("key")
        .ok_or_else(|| WombatError::configuration("Apt source prerequisite lacks `key`"))?
    else {
        return Err(WombatError::configuration(
            "Apt source prerequisite key must be a map",
        ));
    };
    let key_string = |name: &str| match key.get(name) {
        Some(FrozenValue::String(value)) => Ok(value.clone()),
        _ => Err(WombatError::configuration(format!(
            "Apt source prerequisite key lacks `{name}`"
        ))),
    };
    let source = AptSource {
        name: string("name")?,
        uri: string("uri")?,
        suite: string("suite")?,
        components: strings("components", true)?,
        architectures: strings("architectures", false)?,
        key_url: key_string("url")?,
        key_format: key_string("format")?,
        key_sha256: match key.get("sha256") {
            None => None,
            Some(FrozenValue::String(value)) => Some(value.clone()),
            Some(_) => {
                return Err(WombatError::configuration(
                    "Apt source prerequisite key sha256 must be a string",
                ));
            }
        },
        replace: match data.get("replace") {
            Some(FrozenValue::Boolean(value)) => *value,
            _ => {
                return Err(WombatError::configuration(
                    "Apt source prerequisite `replace` must be boolean",
                ));
            }
        },
    };
    if prerequisite.identity != format!("source:{}", source.name) {
        return Err(WombatError::configuration(
            "Apt source prerequisite identity does not match its source name",
        ));
    }
    validate_apt_source(&source, data, key)?;
    Ok(source)
}

fn validate_apt_source(
    source: &AptSource,
    data: &BTreeMap<String, FrozenValue>,
    key: &BTreeMap<String, FrozenValue>,
) -> Result<()> {
    let expected_data = [
        "architectures",
        "components",
        "key",
        "name",
        "replace",
        "suite",
        "uri",
    ];
    if let Some(field) = data
        .keys()
        .find(|field| !expected_data.contains(&field.as_str()))
    {
        return Err(WombatError::configuration(format!(
            "Apt source prerequisite does not support `{field}`"
        )));
    }
    let expected_key = ["format", "sha256", "url"];
    if let Some(field) = key
        .keys()
        .find(|field| !expected_key.contains(&field.as_str()))
    {
        return Err(WombatError::configuration(format!(
            "Apt source prerequisite key does not support `{field}`"
        )));
    }
    if source.name.len() > 64
        || !source
            .name
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_lowercase)
        || !source.name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
    {
        return Err(WombatError::configuration("Apt source name is invalid"));
    }
    validate_http_url(&source.uri, "Apt source uri")?;
    validate_http_url(&source.key_url, "Apt source key url")?;
    if source.key_sha256.is_none() && !source.key_url.starts_with("https://") {
        return Err(WombatError::configuration(
            "Apt source key requires HTTPS unless sha256 is supplied",
        ));
    }
    if source.suite.is_empty() || !single_token(&source.suite) {
        return Err(WombatError::configuration("Apt source suite is invalid"));
    }
    validate_sorted_tokens(&source.components, false, "Apt source components")?;
    validate_sorted_tokens(&source.architectures, true, "Apt source architectures")?;
    if !matches!(source.key_format.as_str(), "gpg" | "asc") {
        return Err(WombatError::configuration(
            "Apt source key format must be `gpg` or `asc`",
        ));
    }
    if source.key_sha256.as_ref().is_some_and(|digest| {
        digest.len() != 64
            || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            || digest.bytes().any(|byte| byte.is_ascii_uppercase())
    }) {
        return Err(WombatError::configuration(
            "Apt source key sha256 must be 64 lowercase hexadecimal digits",
        ));
    }
    Ok(())
}

fn validate_http_url(value: &str, label: &str) -> Result<()> {
    let parsed = url::Url::parse(value)
        .map_err(|error| WombatError::configuration(format!("{label} is invalid: {error}")))?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
    {
        return Err(WombatError::configuration(format!(
            "{label} must be an HTTP or HTTPS URL without credentials or a fragment"
        )));
    }
    Ok(())
}

fn single_token(value: &str) -> bool {
    !value.is_empty()
        && !value
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
}

fn validate_sorted_tokens(values: &[String], allow_empty: bool, label: &str) -> Result<()> {
    if (!allow_empty && values.is_empty())
        || values.iter().any(|value| !single_token(value))
        || !values.windows(2).all(|pair| pair[0] < pair[1])
    {
        return Err(WombatError::configuration(format!(
            "{label} must contain uniquely sorted non-empty tokens"
        )));
    }
    Ok(())
}

pub(super) fn validate_builtin_contracts(
    requirements: &[Requirement],
    prerequisites: &[ProviderPrerequisite],
    preparations: &[ProviderPreparation],
) -> Result<()> {
    let apt_prerequisites = prerequisites
        .iter()
        .filter(|prerequisite| prerequisite.provider == "apt")
        .map(|prerequisite| {
            if !prerequisite.elevated {
                return Err(WombatError::configuration(
                    "Apt source prerequisites must declare elevation",
                ));
            }
            Ok((prerequisite.identity.as_str(), apt_source(prerequisite)?))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    for requirement in requirements
        .iter()
        .filter(|requirement| requirement.binding.provider == "apt")
    {
        let binding = &requirement.binding;
        let FrozenValue::Map(data) = &binding.data else {
            return Err(WombatError::configuration("Apt binding data must be a map"));
        };
        if let Some(field) = data
            .keys()
            .find(|field| !matches!(field.as_str(), "name" | "source"))
        {
            return Err(WombatError::configuration(format!(
                "Apt binding does not support `{field}`"
            )));
        }
        let package = apt_identity(binding)?;
        if binding.identity != format!("package:{package}") {
            return Err(WombatError::configuration(
                "Apt binding identity does not match its package name",
            ));
        }
        match data.get("source") {
            None if binding.prerequisites.is_empty() => {}
            Some(FrozenValue::String(source))
                if binding.prerequisites == [format!("source:{source}")]
                    && apt_prerequisites.contains_key(format!("source:{source}").as_str()) => {}
            _ => {
                return Err(WombatError::configuration(
                    "Apt binding source and prerequisite identities are inconsistent",
                ));
            }
        }
    }
    if prerequisites.iter().any(|prerequisite| {
        prerequisite.provider != "apt" && matches_builtin_name(&prerequisite.provider)
    }) {
        return Err(WombatError::configuration(
            "only the built-in Apt provider supports prerequisites",
        ));
    }
    for operation in preparations
        .iter()
        .filter(|operation| operation.provider == "apt")
    {
        let FrozenValue::Map(data) = &operation.data else {
            return Err(WombatError::configuration(
                "Apt preparation data must be a map",
            ));
        };
        if operation.identity != "update-index"
            || !operation.elevated
            || data.len() != 1
            || !matches!(data.get("forced"), Some(FrozenValue::Boolean(_)))
        {
            return Err(WombatError::configuration(
                "Apt preparation must be the elevated `update-index` operation with boolean `forced` policy",
            ));
        }
    }
    Ok(())
}

fn matches_builtin_name(name: &str) -> bool {
    matches!(name, "apt" | "brew" | "git")
}

pub(super) fn check_apt_source(
    context: &RequirementContext<'_>,
    prerequisite: &ProviderPrerequisite,
) -> Result<(CheckStatus, String)> {
    let source = apt_source(prerequisite)?;
    let source_path = context.system_root.join(source.source_relative_path());
    let key_path = context.system_root.join(source.key_relative_path());
    for parent in [source_path.parent(), key_path.parent()]
        .into_iter()
        .flatten()
    {
        if !plain_directory_or_missing(parent)? {
            return Ok((
                CheckStatus::Unavailable,
                format!(
                    "{} is not a plain directory; Apt source publication is unsafe",
                    parent.display()
                ),
            ));
        }
    }
    let expected = source.deb822();
    let source_bytes = read_optional(&source_path)?;
    let key_bytes = read_optional(&key_path)?;
    let owned = source_bytes
        .as_deref()
        .is_some_and(|bytes| bytes.starts_with(source.marker().as_bytes()));
    if source_bytes.as_deref() != Some(expected.as_bytes()) {
        if source_bytes.is_some() && !owned && !source.replace {
            return Ok((
                CheckStatus::Unavailable,
                format!(
                    "{} contains unmanaged conflicting content; set replace = true to adopt it",
                    source_path.display()
                ),
            ));
        }
        if source_bytes.is_none() && key_bytes.is_some() && !source.replace {
            return Ok((
                CheckStatus::Unavailable,
                format!(
                    "{} exists without Wombat's source marker; set replace = true to adopt it",
                    key_path.display()
                ),
            ));
        }
        return Ok((
            if source_bytes.is_some() {
                CheckStatus::Outdated
            } else {
                CheckStatus::Missing
            },
            format!("Apt source {} needs reconciliation", source.name),
        ));
    }
    let Some(key_bytes) = key_bytes else {
        return Ok((
            CheckStatus::Missing,
            format!("Apt source {} signing key is absent", source.name),
        ));
    };
    if !apt_key_is_usable(&source, &key_bytes) {
        return Ok((
            CheckStatus::Outdated,
            format!("Apt source {} signing key is invalid", source.name),
        ));
    }
    if !plain_file_mode(&source_path, 0o644)? || !plain_file_mode(&key_path, 0o644)? {
        return Ok((
            CheckStatus::Outdated,
            format!("Apt source {} files need mode 0644", source.name),
        ));
    }
    Ok((
        CheckStatus::Satisfied,
        format!("Apt source {} is configured", source.name),
    ))
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(WombatError::io(path, error)),
    }
}

fn apt_key_is_usable(source: &AptSource, bytes: &[u8]) -> bool {
    if bytes.is_empty() || bytes.len() > OUTPUT_LIMIT {
        return false;
    }
    let armored = bytes.starts_with(b"-----BEGIN PGP PUBLIC KEY BLOCK-----");
    if (source.key_format == "asc") != armored {
        return false;
    }
    source
        .key_sha256
        .as_ref()
        .is_none_or(|expected| crate::storage::digest::hex_sha256(bytes) == *expected)
}

fn apt_source_needs_download(context: &RequirementContext<'_>, source: &AptSource) -> Result<bool> {
    Ok(
        read_optional(&context.system_root.join(source.key_relative_path()))?
            .as_deref()
            .is_none_or(|bytes| !apt_key_is_usable(source, bytes)),
    )
}

fn plain_file_mode(path: &Path, expected: u32) -> Result<bool> {
    let metadata = fs::symlink_metadata(path).map_err(|error| WombatError::io(path, error))?;
    if !metadata.file_type().is_file() {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        Ok(metadata.permissions().mode() & 0o777 == expected)
    }
    #[cfg(not(unix))]
    {
        let _ = expected;
        Ok(true)
    }
}

fn plain_directory_or_missing(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_dir()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(WombatError::io(path, error)),
    }
}

pub(super) fn reconcile_apt_source(
    context: &RequirementContext<'_>,
    prerequisite: &ProviderPrerequisite,
    noninteractive: bool,
) -> Result<()> {
    let source = apt_source(prerequisite)?;
    let (status, detail) = check_apt_source(context, prerequisite)?;
    if status == CheckStatus::Unavailable {
        return Err(WombatError::configuration(detail));
    }
    if status == CheckStatus::Satisfied {
        return Ok(());
    }

    let key_path = context.system_root.join(source.key_relative_path());
    let source_path = context.system_root.join(source.source_relative_path());
    let existing_key = read_optional(&key_path)?;
    let mut downloaded_key = None;
    let key_bytes = if let Some(bytes) = existing_key
        .as_deref()
        .filter(|bytes| apt_key_is_usable(&source, bytes))
    {
        bytes.to_vec()
    } else {
        let curl = require_command("curl", "Apt source key download")?;
        let download = download_apt_key(&curl, &source, noninteractive)?;
        let bytes =
            fs::read(download.path()).map_err(|error| WombatError::io(download.path(), error))?;
        downloaded_key = Some(download);
        bytes
    };

    let mut source_file = tempfile::NamedTempFile::new()
        .map_err(|error| WombatError::io(std::env::temp_dir(), error))?;
    std::io::Write::write_all(&mut source_file, source.deb822().as_bytes())
        .map_err(|error| WombatError::io(source_file.path(), error))?;
    source_file
        .as_file()
        .sync_all()
        .map_err(|error| WombatError::io(source_file.path(), error))?;

    let key_parent = key_path
        .parent()
        .ok_or_else(|| WombatError::configuration("Apt key path has no parent"))?;
    let source_parent = source_path
        .parent()
        .ok_or_else(|| WombatError::configuration("Apt source path has no parent"))?;
    let install = require_command("install", "Apt source installation")?;
    let mv = require_command("mv", "Apt source publication")?;
    let rm = require_command("rm", "Apt source publication cleanup")?;
    let elevated = prerequisite.elevated && context.system_root == Path::new("/");
    for parent in [key_parent, source_parent] {
        if !plain_directory_or_missing(parent)? {
            return Err(WombatError::configuration(format!(
                "{} is not a plain directory; Apt source publication is unsafe",
                parent.display()
            )));
        }
        if parent.exists() {
            continue;
        }
        run_mutating(
            &install,
            &["-d", "-m", "0755", &parent.to_string_lossy()],
            &BTreeMap::new(),
            elevated,
            noninteractive,
        )?;
    }
    let nonce = std::process::id();
    let key_staging = key_path.with_extension(format!("{}.wombat-new-{nonce}", source.key_format));
    let source_staging = source_path.with_extension(format!("sources.wombat-new-{nonce}"));
    let key_input = if let Some(download) = &downloaded_key {
        download.path()
    } else {
        key_path.as_path()
    };
    let publish_key = !plain_file_mode_or_missing(&key_path, 0o644)?
        || read_optional(&key_path)?.as_deref() != Some(key_bytes.as_slice());
    let publish_source = !plain_file_mode_or_missing(&source_path, 0o644)?
        || read_optional(&source_path)?.as_deref() != Some(source.deb822().as_bytes());
    let publications = [
        publish_key.then_some((key_input, key_staging.as_path(), key_path.as_path())),
        publish_source.then_some((
            source_file.path(),
            source_staging.as_path(),
            source_path.as_path(),
        )),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    for (input, staging, _) in &publications {
        if let Err(error) = run_mutating(
            &install,
            &[
                "-m",
                "0644",
                &input.to_string_lossy(),
                &staging.to_string_lossy(),
            ],
            &BTreeMap::new(),
            elevated,
            noninteractive,
        ) {
            for (_, staged, _) in &publications {
                cleanup_apt_staging(&rm, staged, elevated, noninteractive);
            }
            return Err(error);
        }
    }
    let mut published = Vec::new();
    for (_, staging, final_path) in &publications {
        if let Err(error) = run_mutating(
            &mv,
            &[
                "-f",
                "--",
                &staging.to_string_lossy(),
                &final_path.to_string_lossy(),
            ],
            &BTreeMap::new(),
            elevated,
            noninteractive,
        ) {
            for (_, staged, _) in &publications {
                cleanup_apt_staging(&rm, staged, elevated, noninteractive);
            }
            return Err(error.with_note(format!(
                "Apt source `{}` publication completed: {}; remaining files were not rolled back",
                source.name,
                if published.is_empty() {
                    "none".to_string()
                } else {
                    published.join(", ")
                }
            )));
        }
        published.push(final_path.display().to_string());
    }
    Ok(())
}

fn download_apt_key(
    curl: &Path,
    source: &AptSource,
    noninteractive: bool,
) -> Result<tempfile::NamedTempFile> {
    let download = tempfile::NamedTempFile::new()
        .map_err(|error| WombatError::io(std::env::temp_dir(), error))?;
    let output = download.path().to_string_lossy().into_owned();
    let protocols = if source.key_sha256.is_some() {
        "=http,https"
    } else {
        "=https"
    };
    run_mutating(
        curl,
        &[
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--connect-timeout",
            "10",
            "--max-time",
            "60",
            "--max-filesize",
            "4194304",
            "--proto",
            protocols,
            "--proto-redir",
            protocols,
            "--output",
            &output,
            &source.key_url,
        ],
        &BTreeMap::new(),
        false,
        noninteractive,
    )?;
    let bytes =
        fs::read(download.path()).map_err(|error| WombatError::io(download.path(), error))?;
    if !apt_key_is_usable(source, &bytes) {
        let observed = crate::storage::digest::hex_sha256(&bytes);
        return Err(WombatError::configuration(format!(
            "Apt source `{}` downloaded an invalid {} signing key ({} bytes, sha256 {observed})",
            source.name,
            source.key_format,
            bytes.len()
        )));
    }
    Ok(download)
}

fn cleanup_apt_staging(rm: &Path, staging: &Path, elevated: bool, noninteractive: bool) {
    let _ = run_mutating(
        rm,
        &["-f", "--", &staging.to_string_lossy()],
        &BTreeMap::new(),
        elevated,
        noninteractive,
    );
}

fn plain_file_mode_or_missing(path: &Path, expected: u32) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => plain_file_mode(path, expected),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(WombatError::io(path, error)),
    }
}

pub(super) fn brew_identity(binding: &ProviderBinding) -> Result<(&str, &str)> {
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

pub(super) fn git_identity(binding: &ProviderBinding) -> Result<(&str, &str, Option<&str>)> {
    let FrozenValue::Map(data) = &binding.data else {
        return Err(WombatError::configuration("Git binding data must be a map"));
    };
    let repository = match data.get("repository") {
        Some(FrozenValue::String(value)) => value.as_str(),
        _ => return Err(WombatError::configuration("Git binding lacks repository")),
    };
    let to = match data.get("to") {
        Some(FrozenValue::String(value)) => value.as_str(),
        _ => return Err(WombatError::configuration("Git binding lacks destination")),
    };
    let reference = match data.get("ref") {
        None => None,
        Some(FrozenValue::String(value)) => Some(value.as_str()),
        _ => return Err(WombatError::configuration("Git binding has an invalid ref")),
    };
    Ok((repository, to, reference))
}

/// Reports whether `to` is already a checkout of `repository`. An existing
/// directory that isn't is left untouched rather than reused or replaced —
/// only an absent destination is safe to clone into.
pub(super) fn confirm_or_absent_git_checkout(
    git: &Path,
    to: &str,
    repository: &str,
) -> Result<bool> {
    if !Path::new(to).join(".git").is_dir() {
        return Ok(false);
    }
    let remote = run_bounded(
        git,
        &["-C", to, "remote", "get-url", "origin"],
        &BTreeMap::new(),
    )?;
    let observed = String::from_utf8_lossy(&remote.stdout.bytes)
        .trim()
        .to_string();
    if !remote.success || observed != repository {
        return Err(WombatError::configuration(format!(
            "{to} already exists and is not a checkout of `{repository}`; resolve it manually"
        )));
    }
    Ok(true)
}

pub(super) fn brew_flag(kind: &str) -> &'static str {
    if kind == "cask" {
        "--cask"
    } else {
        "--formula"
    }
}

pub(super) fn brew_operation(binding: &ProviderBinding) -> Result<&'static str> {
    match check_brew(binding, None, &BrewSnapshot::fetch(&[])?)?.status {
        CheckStatus::Satisfied | CheckStatus::Outdated => Ok("upgrade"),
        CheckStatus::Missing => Ok("install"),
        CheckStatus::Unavailable => Err(WombatError::configuration(
            "Homebrew package state became unavailable during bootstrap",
        )),
    }
}

pub(super) fn installed_brew_versions(entry: &serde_json::Value) -> Vec<String> {
    match entry.get("installed") {
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
    }
}

pub(super) fn brew_environment() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("HOMEBREW_NO_AUTO_UPDATE".to_string(), "1".to_string()),
        ("HOMEBREW_NO_INSTALL_CLEANUP".to_string(), "1".to_string()),
    ])
}

pub(super) fn apt_environment() -> BTreeMap<String, String> {
    BTreeMap::from([("DEBIAN_FRONTEND".to_string(), "noninteractive".to_string())])
}

pub(super) fn require_command(command: &str, purpose: &str) -> Result<PathBuf> {
    which(command).ok_or_else(|| {
        WombatError::configuration(format!(
            "{purpose} requires `{command}` to be available on PATH"
        ))
    })
}

pub(super) fn effective_uid_is_root() -> Result<bool> {
    let id = which("id").unwrap_or_else(|| PathBuf::from("/usr/bin/id"));
    let output = run_bounded(&id, &["-u"], &BTreeMap::new())?;
    if !output.success {
        return Err(WombatError::configuration(format!(
            "could not determine the effective user: {}",
            output_detail(&output)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout.bytes).trim() == "0")
}

pub(super) fn preflight_elevation(elevated: bool) -> Result<()> {
    if elevated && !effective_uid_is_root()? {
        require_command("sudo", "elevated bootstrap")?;
    }
    Ok(())
}

pub(super) fn authorize_elevation(noninteractive: bool) -> Result<()> {
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
    let status = crate::execution::process::run_inherited(&mut command, "sudo authorization")?;
    if !status.success {
        return Err(WombatError::configuration(if noninteractive {
            "non-interactive bootstrap requires existing passwordless sudo authorization"
        } else {
            "sudo authorization failed"
        }));
    }
    Ok(())
}

pub(super) fn mutating_status(
    program: &Path,
    args: &[&str],
    environment: &BTreeMap<String, String>,
    elevated: bool,
    noninteractive: bool,
) -> Result<ProcessOutcome> {
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
    crate::execution::process::run_inherited(&mut command, &program.display().to_string())
}

pub(super) fn run_mutating(
    program: &Path,
    args: &[&str],
    environment: &BTreeMap<String, String>,
    elevated: bool,
    noninteractive: bool,
) -> Result<()> {
    let status = mutating_status(program, args, environment, elevated, noninteractive)?;
    if status.success {
        Ok(())
    } else {
        Err(WombatError::configuration(format!(
            "provider command `{}` failed with {}",
            program.display(),
            status.status
        )))
    }
}

pub(super) fn observe_command_version(path: &Path) -> Result<String> {
    let output = run_bounded(path, &["--version"], &BTreeMap::new())?;
    if !output.success {
        return Err(WombatError::configuration(format!(
            "version probe `{}` failed: {}",
            path.display(),
            output_detail(&output)
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout.bytes);
    let stderr = String::from_utf8_lossy(&output.stderr.bytes);
    first_version(&stdout)
        .or_else(|| first_version(&stderr))
        .ok_or_else(|| {
            WombatError::configuration(format!(
                "could not parse a version from `{}`",
                path.display()
            ))
        })
}

pub(super) fn first_version(text: &str) -> Option<String> {
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

pub(super) fn version_at_least(observed: &str, minimum: &str) -> bool {
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

pub(super) fn which(command: &str) -> Option<PathBuf> {
    if command.contains(std::path::MAIN_SEPARATOR) {
        let path = PathBuf::from(command);
        return is_executable_file(&path).then_some(path);
    }
    env::split_paths(&env::var_os("PATH")?).find_map(|directory| {
        let path = directory.join(command);
        is_executable_file(&path).then_some(path)
    })
}

pub(super) fn is_executable_file(path: &Path) -> bool {
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

pub(super) fn run_bounded(
    path: &Path,
    args: &[&str],
    environment: &BTreeMap<String, String>,
) -> Result<ProcessOutcome> {
    let mut command = Command::new(path);
    command.args(args).envs(environment);
    let output = crate::execution::process::run(
        &mut command,
        &path.display().to_string(),
        None,
        OUTPUT_LIMIT,
        None,
        crate::execution::process::Forwarding::Retained,
    )?;
    if output.stdout.truncated || output.stderr.truncated {
        return Err(WombatError::configuration(format!(
            "process `{}` exceeded the {} byte observation limit",
            path.display(),
            OUTPUT_LIMIT
        )));
    }
    Ok(output)
}

pub(super) fn output_detail(output: &ProcessOutcome) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr.bytes);
    let stdout = String::from_utf8_lossy(&output.stdout.bytes);
    stderr
        .trim()
        .lines()
        .next()
        .or_else(|| stdout.trim().lines().next())
        .unwrap_or("no diagnostic output")
        .to_string()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::execution::ladder::CoreRung;
    use std::os::unix::fs::PermissionsExt as _;

    fn prerequisite(replace: bool, digest: Option<&str>) -> ProviderPrerequisite {
        let mut data = serde_json::json!({
            "name": "yazi",
            "uri": "https://yazi-rs.github.io/builds/",
            "suite": "stable",
            "components": ["main"],
            "key": {
                "url": "https://yazi-rs.github.io/builds/yazi-keyring.gpg",
                "format": "gpg",
            },
            "replace": replace,
        });
        if let Some(digest) = digest {
            data["key"]["sha256"] = serde_json::Value::String(digest.to_string());
        }
        ProviderPrerequisite {
            provider: "apt".to_string(),
            identity: "source:yazi".to_string(),
            description: "Configure Apt source yazi".to_string(),
            when: CoreRung::DeployBefore.into(),
            elevated: true,
            data: serde_json::from_value(data).unwrap(),
        }
    }

    fn context<'a>(
        root: &Path,
        prerequisites: &'a [ProviderPrerequisite],
    ) -> RequirementContext<'a> {
        RequirementContext {
            id: "fixture",
            providers: &[],
            requirements: &[],
            prerequisites,
            preparations: &[],
            payload_root: root.join("payloads"),
            system_root: root.to_path_buf(),
        }
    }

    #[test]
    fn apt_source_reconciliation_is_rooted_canonical_and_idempotent() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let prerequisite = prerequisite(true, None);
        let source = apt_source(&prerequisite).unwrap();
        let key_path = root.join(source.key_relative_path());
        fs::create_dir_all(key_path.parent().unwrap()).unwrap();
        fs::write(&key_path, b"binary signing key").unwrap();
        fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600)).unwrap();
        let prerequisites = [prerequisite];
        let context = context(root, &prerequisites);

        assert_eq!(
            check_apt_source(&context, &prerequisites[0]).unwrap().0,
            CheckStatus::Missing
        );
        reconcile_apt_source(&context, &prerequisites[0], true).unwrap();
        assert_eq!(
            check_apt_source(&context, &prerequisites[0]).unwrap().0,
            CheckStatus::Satisfied
        );
        assert_eq!(fs::read(&key_path).unwrap(), b"binary signing key");
        let source_path = root.join(source.source_relative_path());
        assert_eq!(fs::read_to_string(&source_path).unwrap(), source.deb822());
        assert_eq!(
            fs::metadata(&key_path).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(
            fs::metadata(&source_path).unwrap().permissions().mode() & 0o777,
            0o644
        );
        reconcile_apt_source(&context, &prerequisites[0], true).unwrap();
        assert!(
            !root
                .join("etc/apt/sources.list.d")
                .read_dir()
                .unwrap()
                .any(|entry| entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains("wombat-new"))
        );
    }

    #[test]
    fn unmanaged_source_requires_explicit_replacement() {
        let temporary = tempfile::tempdir().unwrap();
        let prerequisite = prerequisite(false, None);
        let source = apt_source(&prerequisite).unwrap();
        let source_path = temporary.path().join(source.source_relative_path());
        fs::create_dir_all(source_path.parent().unwrap()).unwrap();
        fs::write(&source_path, "Types: deb\nURIs: https://example.invalid\n").unwrap();
        let prerequisites = [prerequisite];
        let context = context(temporary.path(), &prerequisites);
        let (status, detail) = check_apt_source(&context, &prerequisites[0]).unwrap();
        assert_eq!(status, CheckStatus::Unavailable);
        assert!(detail.contains("replace = true"), "{detail}");
    }

    #[test]
    fn apt_key_download_uses_bounded_protocol_arguments_and_verifies_digest() {
        let temporary = tempfile::tempdir().unwrap();
        let script = temporary.path().join("curl-fixture");
        let log = temporary.path().join("arguments");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nout=''\nwhile [ \"$#\" -gt 0 ]; do\n  printf '%s\\n' \"$1\" >> '{}'\n  if [ \"$1\" = '--output' ]; then out=$2; shift 2; else shift; fi\ndone\nprintf 'binary signing key' > \"$out\"\n",
                log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let digest = crate::storage::digest::hex_sha256(b"binary signing key");
        let prerequisite = prerequisite(true, Some(&digest));
        let mut source = apt_source(&prerequisite).unwrap();
        source.key_url = "http://example.invalid/key.gpg".to_string();
        let download = download_apt_key(&script, &source, true).unwrap();
        assert_eq!(fs::read(download.path()).unwrap(), b"binary signing key");
        let arguments = fs::read_to_string(&log).unwrap();
        assert!(arguments.contains("--max-filesize"));
        assert!(arguments.contains("4194304"));
        assert!(arguments.contains("=http,https"));

        source.key_sha256 = Some("0".repeat(64));
        let error = download_apt_key(&script, &source, true)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("downloaded an invalid gpg signing key"),
            "{error}"
        );
    }

    #[test]
    fn apt_key_format_distinguishes_armored_and_binary_material() {
        let prerequisite = prerequisite(true, None);
        let mut source = apt_source(&prerequisite).unwrap();
        assert!(apt_key_is_usable(&source, b"binary signing key"));
        assert!(!apt_key_is_usable(
            &source,
            b"-----BEGIN PGP PUBLIC KEY BLOCK-----\nkey\n"
        ));
        source.key_format = "asc".to_string();
        assert!(!apt_key_is_usable(&source, b"binary signing key"));
        assert!(apt_key_is_usable(
            &source,
            b"-----BEGIN PGP PUBLIC KEY BLOCK-----\nkey\n"
        ));
    }
}
