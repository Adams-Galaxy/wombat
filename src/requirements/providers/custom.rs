//! Packaged custom-provider loading and constrained Lua execution.

use super::*;

pub(crate) fn check_custom(
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

pub(crate) fn check_custom_prerequisite(
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

pub(crate) fn prepare_custom(
    context: &RequirementContext<'_>,
    provider: &Provider,
    operation: &ProviderPreparation,
    noninteractive: bool,
) -> Result<()> {
    let runtime = CustomRuntime::load(context, provider, true, operation.elevated, noninteractive)?;
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

pub(crate) fn reconcile_custom_prerequisite(
    context: &RequirementContext<'_>,
    provider: &Provider,
    prerequisite: &ProviderPrerequisite,
    observation: &CheckItem,
    noninteractive: bool,
) -> Result<()> {
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

pub(crate) fn preflight_custom_prerequisite(
    context: &RequirementContext<'_>,
    provider: &Provider,
    prerequisite: &ProviderPrerequisite,
) -> Result<()> {
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
    preflight_elevation(prerequisite.elevated)
}

pub(crate) fn preflight_custom_preparation(
    context: &RequirementContext<'_>,
    provider: &Provider,
    operation: &ProviderPreparation,
) -> Result<()> {
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
    preflight_elevation(operation.elevated)
}

pub(crate) fn preflight_custom_requirement(
    context: &RequirementContext<'_>,
    provider: &Provider,
) -> Result<()> {
    CustomRuntime::load(context, provider, false, false, false).map(|_| ())
}

pub(crate) fn reconcile_custom_requirement(
    context: &RequirementContext<'_>,
    provider: &Provider,
    requirement: &Requirement,
    status: CheckStatus,
    noninteractive: bool,
) -> Result<()> {
    let runtime = CustomRuntime::load(
        context,
        provider,
        true,
        requirement.binding.elevated,
        noninteractive,
    )?;
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
                table.set("success", status.success)?;
                table.set("code", status.code)?;
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

fn output_table(lua: &Lua, output: &ProcessOutcome) -> mlua::Result<Table> {
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

fn parse_custom_status(binding: &ProviderBinding, value: FrozenValue) -> Result<CheckItem> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutation_cannot_elevate_without_the_frozen_capability() {
        let lua = Lua::new();
        let context = process_context(&lua, true, false, true).unwrap();
        let mutate: Function = context.get("mutate").unwrap();
        let specification = lua.create_table().unwrap();
        specification.set("program", "/bin/true").unwrap();
        specification.set("elevated", true).unwrap();
        let error = mutate
            .call::<Value>((context, specification))
            .unwrap_err()
            .to_string();
        assert!(error.contains("did not declare elevation"), "{error}");
    }
}
