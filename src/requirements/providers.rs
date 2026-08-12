//! Built-in and Lua provider execution and requirement reconciliation.

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

pub(super) fn preflight(
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
                if !output.success {
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
                if !output.success {
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

pub(super) fn provider_item(
    binding: &ProviderBinding,
    status: CheckStatus,
    detail: &str,
) -> CheckItem {
    CheckItem {
        requirement: String::new(),
        provider: binding.provider.clone(),
        status,
        detail: detail.to_string(),
    }
}

pub(super) fn frozen_binding(binding: &ProviderBinding) -> Result<FrozenValue> {
    Ok(serde_json::from_value(serde_json::to_value(binding)?)?)
}

pub(super) fn frozen_preparation(preparation: &ProviderPreparation) -> Result<FrozenValue> {
    Ok(serde_json::from_value(serde_json::to_value(preparation)?)?)
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

pub(super) fn brew_flag(kind: &str) -> &'static str {
    if kind == "cask" {
        "--cask"
    } else {
        "--formula"
    }
}

pub(super) fn brew_operation(binding: &ProviderBinding) -> Result<&'static str> {
    match check_brew(binding, None)?.status {
        CheckStatus::Satisfied | CheckStatus::Outdated => Ok("upgrade"),
        CheckStatus::Missing => Ok("install"),
        CheckStatus::Unavailable => Err(WombatError::configuration(
            "Homebrew package state became unavailable during bootstrap",
        )),
    }
}

pub(super) fn installed_brew_versions(json: &serde_json::Value, kind: &str) -> Result<Vec<String>> {
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
