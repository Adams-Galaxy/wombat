//! Provider loading, requirement resolution, and frozen provider plans.

use super::*;
use crate::requirements::providers::builtin::BuiltinProvider;

pub(super) fn configure_providers(
    state: &Rc<RefCell<RuntimeState>>,
    entries: Value,
    location: Location,
) -> Result<()> {
    let frozen = FrozenValue::from_lua(entries)?;
    let values = match frozen {
        FrozenValue::Array(values) => values,
        FrozenValue::Map(values) if values.is_empty() => Vec::new(),
        _ => {
            return Err(WombatError::configuration(
                "w.providers() requires an array of provider names or provider option tables",
            ));
        }
    };
    let mut configured = Vec::with_capacity(values.len());
    let root = {
        let mut state = state.borrow_mut();
        if state.active_module().is_some() {
            return Err(WombatError::configuration(format!(
                "{} belongs to root policy at {}",
                "w.providers()",
                location.display()
            )));
        }
        let already_declared = !state.providers.is_empty();
        if already_declared {
            return Err(WombatError::configuration(format!(
                "{} may be declared only once; repeated at {}",
                "w.providers()",
                location.display()
            )));
        }
        if !state.modules.is_empty()
            || !state.artifacts.is_empty()
            || !state.requirements.is_empty()
            || !state.tasks.is_empty()
        {
            return Err(WombatError::configuration(format!(
                "{} must run before use(), using(), install(), need(), generate(), or build.task() at {}",
                "w.providers()",
                location.display()
            )));
        }
        state.root_policy_started = true;
        state.root.clone()
    };

    let mut names = BTreeSet::new();
    for (index, value) in values.into_iter().enumerate() {
        let (name, config) = match value {
            FrozenValue::String(name) => (name, FrozenValue::empty_map()),
            FrozenValue::Map(mut options) => {
                let name = take_string(&mut options, "name", "provider")?;
                let config = options
                    .remove("with")
                    .unwrap_or_else(FrozenValue::empty_map);
                if !matches!(config, FrozenValue::Map(_)) {
                    return Err(WombatError::configuration(format!(
                        "provider `{name}` requires map-shaped `with` options"
                    )));
                }
                reject_unknown_options(&options, "provider")?;
                (name, config)
            }
            _ => {
                return Err(WombatError::configuration(format!(
                    "provider entry {} must be a string or table",
                    index + 1
                )));
            }
        };
        validate_provider_name(&name)?;
        if !names.insert(name.clone()) {
            return Err(WombatError::configuration(format!(
                "provider `{name}` is configured more than once"
            )));
        }
        let origin = if let Some(builtin) = BuiltinProvider::from_name(&name) {
            let conflicting = root.join("providers").join(format!("{name}.lua"));
            if conflicting.exists() {
                return Err(WombatError::configuration(format!(
                    "custom provider `providers/{name}.lua` conflicts with reserved built-in provider `{name}`"
                )));
            }
            ProviderOrigin::Builtin {
                contract_version: builtin.contract_version(),
            }
        } else {
            let entrypoint = root.join("providers").join(format!("{name}.lua"));
            let snapshot = load_tracked_source(state, &entrypoint).map_err(|error| {
                error.with_note(format!(
                    "custom provider `{name}` must be defined at `providers/{name}.lua`"
                ))
            })?;
            let source_path = format!("providers/{name}.lua");
            let digest = state
                .borrow()
                .sources
                .get(&source_path)
                .expect("loaded provider source must be tracked")
                .manifest
                .digest
                .clone();
            ProviderOrigin::Custom {
                entrypoint: format!("{name}.lua"),
                files: vec![crate::model::manifest::ProviderFile {
                    source: source_path,
                    payload: format!("{name}.lua"),
                    digest,
                    size: u64::try_from(snapshot.len())
                        .map_err(|_| WombatError::configuration("provider source is too large"))?,
                }],
            }
        };
        configured.push(Provider {
            name,
            priority: u32::try_from(index)
                .map_err(|_| WombatError::configuration("too many configured providers"))?,
            config,
            origin,
            declared_at: location.trace.clone(),
        });
    }
    let custom_names = configured
        .iter()
        .filter(|provider| matches!(provider.origin, ProviderOrigin::Custom { .. }))
        .map(|provider| provider.name.clone())
        .collect::<Vec<_>>();
    state.borrow_mut().providers = configured;
    for name in custom_names {
        validate_custom_provider(state, &name)?;
        record_provider_sources(state, &name)?;
    }
    Ok(())
}

pub(super) struct RequirementDeclaration<'a> {
    pub(super) kind: &'a str,
    pub(super) name: &'a str,
    pub(super) options: Value,
    pub(super) preferred: bool,
    pub(super) location: Location,
}

pub(super) fn declare_requirement(
    lua: &Lua,
    state: &Rc<RefCell<RuntimeState>>,
    declaration: RequirementDeclaration<'_>,
) -> Result<Value> {
    let RequirementDeclaration {
        kind,
        name,
        options,
        preferred,
        location,
    } = declaration;
    let options = if options.is_nil() {
        FrozenValue::empty_map()
    } else {
        FrozenValue::from_lua(options)?
    };
    let FrozenValue::Map(mut options) = options else {
        return Err(WombatError::configuration(format!(
            "w.{}.{}() options must be a table",
            if preferred { "prefer" } else { "need" },
            kind
        )));
    };
    let requirement_kind = match kind {
        "command" => RequirementKind::Command,
        "package" => RequirementKind::Package,
        _ => {
            return Err(WombatError::configuration(format!(
                "unknown requirement kind `{kind}`"
            )));
        }
    };
    let mut candidates = vec![parse_requirement_candidate(
        requirement_kind,
        name,
        &mut options,
        false,
    )?];
    let when = match options.remove("when") {
        None => CoreRung::MaterialiseBefore.into(),
        Some(FrozenValue::String(value)) => RungId::new(value)?,
        Some(_) => {
            return Err(WombatError::configuration(
                "requirement `when` must be a w.rungs handle or canonical rung string",
            ));
        }
    };
    if let Some(accept) = options.remove("accept") {
        if !preferred {
            return Err(WombatError::configuration(
                "w.need() does not support `accept`; use w.prefer()",
            ));
        }
        let values = match accept {
            FrozenValue::String(name) if requirement_kind == RequirementKind::Command => {
                vec![FrozenValue::String(name)]
            }
            FrozenValue::Map(value) => vec![FrozenValue::Map(value)],
            FrozenValue::Array(values) => values,
            _ => {
                return Err(WombatError::configuration(
                    "prefer `accept` must be a command string, candidate table, or candidate array",
                ));
            }
        };
        if values.is_empty() {
            return Err(WombatError::configuration(
                "prefer `accept` must contain at least one candidate",
            ));
        }
        for value in values {
            match value {
                FrozenValue::String(name) if requirement_kind == RequirementKind::Command => {
                    let mut empty = BTreeMap::new();
                    candidates.push(parse_requirement_candidate(
                        requirement_kind,
                        &name,
                        &mut empty,
                        true,
                    )?);
                }
                FrozenValue::Map(mut candidate) => {
                    let candidate_name = take_string(&mut candidate, "name", "accepted candidate")?;
                    candidates.push(parse_requirement_candidate(
                        requirement_kind,
                        &candidate_name,
                        &mut candidate,
                        true,
                    )?);
                }
                _ => {
                    return Err(WombatError::configuration(
                        "accepted command candidates must be strings or tables and package candidates must be tables",
                    ));
                }
            }
        }
    }
    reject_unknown_options(&options, "requirement")?;

    let (owner, providers, target) = {
        let mut state = state.borrow_mut();
        let providers = state.providers.clone();
        if providers.is_empty() {
            return Err(WombatError::configuration(format!(
                "requirements need provider policy; call w.providers() before {} at {}",
                if preferred { "w.prefer()" } else { "w.need()" },
                location.display()
            )));
        }
        if state.active_module().is_none() {
            state.root_policy_started = true;
        }
        (
            state.active_module().unwrap_or(ROOT_MODULE).to_string(),
            providers,
            effective_target(&state),
        )
    };

    let mut attempts = Vec::new();
    let mut selected = None;
    for (candidate_index, candidate) in candidates.iter().enumerate() {
        if let RequirementCandidate::Package {
            provider: Some(required),
            ..
        } = candidate
            && !providers.iter().any(|provider| provider.name == *required)
        {
            return Err(WombatError::configuration(format!(
                "package candidate `{}` requests provider `{required}`, which is not configured",
                candidate.name()
            )));
        }
        let candidates_providers = providers.iter().filter(|provider| match candidate {
            RequirementCandidate::Command { .. } => true,
            RequirementCandidate::Package {
                provider: Some(required),
                ..
            } => provider.name == *required,
            RequirementCandidate::Package { provider: None, .. } => true,
        });
        for provider in candidates_providers {
            let outcome = resolve_provider_requirement(state, provider, candidate, &target)?;
            let candidate_index = u32::try_from(candidate_index)
                .map_err(|_| WombatError::configuration("too many requirement candidates"))?;
            match outcome {
                Ok(binding) => {
                    attempts.push(ResolutionAttempt {
                        candidate: candidate_index,
                        provider: provider.name.clone(),
                        outcome: ResolutionOutcome::Selected,
                    });
                    selected = Some((candidate_index, binding));
                    break;
                }
                Err(reason) => attempts.push(ResolutionAttempt {
                    candidate: candidate_index,
                    provider: provider.name.clone(),
                    outcome: ResolutionOutcome::Unsupported { reason },
                }),
            }
        }
        if selected.is_some() {
            break;
        }
    }
    let Some((selected_index, binding)) = selected else {
        let reasons = attempts
            .iter()
            .map(|attempt| match &attempt.outcome {
                ResolutionOutcome::Unsupported { reason } => format!(
                    "candidate {} through `{}`: {reason}",
                    attempt.candidate + 1,
                    attempt.provider
                ),
                ResolutionOutcome::Selected => unreachable!(),
            })
            .collect::<Vec<_>>()
            .join("; ");
        return Err(WombatError::configuration(format!(
            "no configured provider can resolve {kind} requirement `{name}` at {}: {reasons}",
            location.display()
        )));
    };
    let choice = if !preferred {
        RequirementChoice::Required
    } else if selected_index == 0 {
        RequirementChoice::Preferred
    } else {
        RequirementChoice::Accepted
    };
    let requirement = Requirement {
        kind: requirement_kind,
        owner,
        declared_at: location.trace,
        candidates,
        attempts,
        selected: selected_index,
        choice,
        binding,
        when,
    };
    let handle = resolved_requirement_handle(&requirement);
    state.borrow_mut().requirements.push(requirement);
    readonly_frozen(lua, handle).map_err(WombatError::from)
}

pub(super) fn parse_requirement_candidate(
    kind: RequirementKind,
    name: &str,
    options: &mut BTreeMap<String, FrozenValue>,
    accepted: bool,
) -> Result<RequirementCandidate> {
    validate_product_name(name, kind)?;
    let minimum = take_optional_string(options, "minimum", "requirement")?;
    if minimum.as_deref().is_some_and(str::is_empty) {
        return Err(WombatError::configuration(
            "requirement minimum version must not be empty",
        ));
    }
    match kind {
        RequirementKind::Command => {
            if accepted {
                reject_unknown_options(options, "accepted command candidate")?;
            }
            Ok(RequirementCandidate::Command {
                name: name.to_string(),
                minimum,
            })
        }
        RequirementKind::Package => {
            let provider = take_optional_string(options, "provider", "package requirement")?;
            if let Some(provider) = &provider {
                validate_provider_name(provider)?;
            }
            let publications = options
                .remove("publishes")
                .map(|value| parse_publications("publishes", value))
                .transpose()?
                .unwrap_or(Publications {
                    commands: Vec::new(),
                });
            let with = options
                .remove("with")
                .unwrap_or_else(FrozenValue::empty_map);
            if !matches!(with, FrozenValue::Map(_)) {
                return Err(WombatError::configuration(
                    "package requirement `with` must be a string-keyed map",
                ));
            }
            if accepted {
                reject_unknown_options(options, "accepted package candidate")?;
            }
            Ok(RequirementCandidate::Package {
                name: name.to_string(),
                provider,
                minimum,
                publications,
                with,
            })
        }
    }
}

pub(super) fn parse_publications(field: &str, value: FrozenValue) -> Result<Publications> {
    let FrozenValue::Map(mut values) = value else {
        return Err(WombatError::configuration(format!(
            "package `{field}` must be a table"
        )));
    };
    let commands = match values.remove("commands") {
        None => Vec::new(),
        Some(FrozenValue::Array(values)) => values
            .into_iter()
            .map(|value| match value {
                FrozenValue::String(command) => {
                    validate_product_name(&command, RequirementKind::Command)?;
                    Ok(command)
                }
                _ => Err(WombatError::configuration(
                    "published commands must be strings",
                )),
            })
            .collect::<Result<Vec<_>>>()?,
        // An empty Lua table `{}` cannot signal "array" on its own, so
        // `FrozenValue::from_lua` always freezes it as an empty map; accept
        // that as an empty command list rather than rejecting it.
        Some(FrozenValue::Map(values)) if values.is_empty() => Vec::new(),
        Some(_) => {
            return Err(WombatError::configuration(format!(
                "package `{field}.commands` must be an array"
            )));
        }
    };
    reject_unknown_options(&values, "package publications")?;
    let mut sorted = commands;
    sorted.sort();
    if sorted.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(WombatError::configuration(
            "published commands must be unique",
        ));
    }
    Ok(Publications { commands: sorted })
}

pub(super) fn resolve_provider_requirement(
    state: &Rc<RefCell<RuntimeState>>,
    provider: &Provider,
    candidate: &RequirementCandidate,
    target: &TargetPlatform,
) -> Result<std::result::Result<ProviderBinding, String>> {
    let lua = Lua::new_with(
        StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8,
        LuaOptions::default(),
    )?;
    for name in ["dofile", "load", "loadfile"] {
        lua.globals().set(name, Value::Nil)?;
    }
    let api = provider_api(&lua)?;
    let source = match &provider.origin {
        ProviderOrigin::Builtin { .. } => {
            builtin_provider(&provider.name)?.lua_source().to_string()
        }
        ProviderOrigin::Custom { entrypoint, .. } => {
            install_provider_require(&lua, state.clone(), &provider.name, api.clone())?;
            let path = state.borrow().root.join("providers").join(entrypoint);
            load_tracked_source(state, &path)?
        }
    };
    if matches!(provider.origin, ProviderOrigin::Builtin { .. }) {
        let api_for_require = api.clone();
        lua.globals().set(
            "require",
            lua.create_function(move |_, name: String| {
                if name == "wombat.provider" {
                    Ok(api_for_require.clone())
                } else {
                    Err(mlua::Error::external(WombatError::configuration(format!(
                        "built-in provider cannot require `{name}`"
                    ))))
                }
            })?,
        )?;
    }
    let definition: Table = lua
        .load(&source)
        .set_name(format!("@providers/{}.lua", provider.name))
        .eval()
        .map_err(|error| provider_lua_error(&provider.name, "load", error))?;
    let resolve: Function = definition.get("resolve").map_err(|error| {
        provider_lua_error(&provider.name, "definition requires resolve", error)
    })?;
    let candidate = frozen_candidate(candidate)?;
    let result: Value = resolve
        .call((
            candidate.to_lua(&lua)?,
            target.to_frozen().to_lua(&lua)?,
            provider.config.to_lua(&lua)?,
        ))
        .map_err(|error| provider_lua_error(&provider.name, "resolve", error))?;
    let frozen = FrozenValue::from_lua(result)?;
    record_provider_sources(state, &provider.name)?;
    parse_provider_resolution(&provider.name, frozen)
}

fn builtin_provider(name: &str) -> Result<BuiltinProvider> {
    BuiltinProvider::from_name(name)
        .ok_or_else(|| WombatError::configuration(format!("unknown built-in provider `{name}`")))
}

pub(super) fn provider_api(lua: &Lua) -> Result<Table> {
    let api = lua.create_table()?;
    api.set(
        "define",
        lua.create_function(|_, definition: Table| Ok(definition))?,
    )?;
    api.set(
        "unsupported",
        lua.create_function(|lua, reason: String| {
            let result = lua.create_table()?;
            result.set("kind", "unsupported")?;
            result.set("reason", reason)?;
            Ok(result)
        })?,
    )?;
    api.set(
        "binding",
        lua.create_function(|_, binding: Table| {
            binding.set("kind", "binding")?;
            Ok(binding)
        })?,
    )?;
    api.set(
        "operation",
        lua.create_function(|_, operation: Table| {
            operation.set("kind", "operation")?;
            Ok(operation)
        })?,
    )?;
    api.set(
        "prerequisite",
        lua.create_function(|_, prerequisite: Table| {
            prerequisite.set("kind", "prerequisite")?;
            Ok(prerequisite)
        })?,
    )?;
    Ok(api)
}

pub(super) fn validate_custom_provider(
    state: &Rc<RefCell<RuntimeState>>,
    name: &str,
) -> Result<()> {
    let lua = Lua::new_with(
        StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8,
        LuaOptions::default(),
    )?;
    for global in ["dofile", "load", "loadfile"] {
        lua.globals().set(global, Value::Nil)?;
    }
    let api = provider_api(&lua)?;
    install_provider_require(&lua, state.clone(), name, api)?;
    let entrypoint = state
        .borrow()
        .root
        .join("providers")
        .join(format!("{name}.lua"));
    let source = load_tracked_source(state, &entrypoint)?;
    let definition: Table = lua
        .load(&source)
        .set_name(format!("@providers/{name}.lua"))
        .eval()
        .map_err(|error| provider_lua_error(name, "load", error))?;
    for operation in ["resolve", "check", "reconcile"] {
        definition.get::<Function>(operation).map_err(|error| {
            provider_lua_error(name, &format!("definition requires {operation}()"), error)
        })?;
    }
    Ok(())
}

pub(super) fn plan_provider_actions(
    state: &Rc<RefCell<RuntimeState>>,
) -> Result<(Vec<ProviderPrerequisite>, Vec<ProviderPreparation>)> {
    let (providers, requirements, target) = {
        let state = state.borrow();
        (
            state.providers.clone(),
            state.requirements.clone(),
            effective_target(&state),
        )
    };
    let mut prerequisites = Vec::new();
    let mut preparations = Vec::new();
    for provider in providers {
        let prerequisite_start = prerequisites.len();
        let preparation_start = preparations.len();
        let lua = Lua::new_with(
            StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8,
            LuaOptions::default(),
        )?;
        for name in ["dofile", "load", "loadfile"] {
            lua.globals().set(name, Value::Nil)?;
        }
        let api = provider_api(&lua)?;
        let source = match &provider.origin {
            ProviderOrigin::Builtin { .. } => {
                builtin_provider(&provider.name)?.lua_source().to_string()
            }
            ProviderOrigin::Custom { entrypoint, .. } => {
                install_provider_require(&lua, Rc::clone(state), &provider.name, api.clone())?;
                let path = state.borrow().root.join("providers").join(entrypoint);
                load_tracked_source(state, &path)?
            }
        };
        if matches!(provider.origin, ProviderOrigin::Builtin { .. }) {
            let api_for_require = api.clone();
            lua.globals().set(
                "require",
                lua.create_function(move |_, name: String| {
                    if name == "wombat.provider" {
                        Ok(api_for_require.clone())
                    } else {
                        Err(mlua::Error::external(WombatError::configuration(format!(
                            "built-in provider cannot require `{name}`"
                        ))))
                    }
                })?,
            )?;
        }
        let definition: Table = lua
            .load(&source)
            .set_name(format!("@providers/{}.lua", provider.name))
            .eval()
            .map_err(|error| provider_lua_error(&provider.name, "load", error))?;
        let Some(plan) = definition
            .get::<Option<Function>>("plan")
            .map_err(|error| provider_lua_error(&provider.name, "plan lookup", error))?
        else {
            continue;
        };
        let mut binding_values = Vec::new();
        for requirement in requirements
            .iter()
            .filter(|requirement| requirement.binding.provider == provider.name)
        {
            binding_values.push(frozen_binding(&requirement.binding)?.to_lua(&lua)?);
        }
        let bindings = lua.create_sequence_from(binding_values)?;
        let value: Value = plan
            .call((
                bindings,
                target.to_frozen().to_lua(&lua)?,
                provider.config.to_lua(&lua)?,
            ))
            .map_err(|error| provider_lua_error(&provider.name, "plan", error))?;
        let operations = match FrozenValue::from_lua(value)? {
            FrozenValue::Array(operations) => operations,
            FrozenValue::Map(values) if values.is_empty() => Vec::new(),
            _ => {
                return Err(WombatError::configuration(format!(
                    "provider `{}` plan() must return an array of provider.operation() or provider.prerequisite() values",
                    provider.name
                )));
            }
        };
        for operation in operations {
            let FrozenValue::Map(values) = &operation else {
                return Err(WombatError::configuration(format!(
                    "provider `{}` plan() entries must be planned provider values",
                    provider.name
                )));
            };
            match values.get("kind") {
                Some(FrozenValue::String(kind)) if kind == "operation" => {
                    preparations.push(parse_provider_operation(&provider.name, operation)?);
                }
                Some(FrozenValue::String(kind)) if kind == "prerequisite" => {
                    prerequisites.push(parse_provider_prerequisite(&provider.name, operation)?);
                }
                Some(FrozenValue::String(kind)) => {
                    return Err(WombatError::configuration(format!(
                        "provider `{}` returned unknown planned value `{kind}`",
                        provider.name
                    )));
                }
                _ => {
                    return Err(WombatError::configuration(format!(
                        "provider `{}` plan() entry lacks a provider value kind",
                        provider.name
                    )));
                }
            }
        }
        if prerequisites.len() > prerequisite_start
            && matches!(provider.origin, ProviderOrigin::Custom { .. })
        {
            for callback in ["check_prerequisite", "reconcile_prerequisite"] {
                definition.get::<Function>(callback).map_err(|error| {
                    provider_lua_error(
                        &provider.name,
                        &format!("planned prerequisites require {callback}()"),
                        error,
                    )
                })?;
            }
        }
        if preparations.len() > preparation_start
            && matches!(provider.origin, ProviderOrigin::Custom { .. })
        {
            definition.get::<Function>("prepare").map_err(|error| {
                provider_lua_error(
                    &provider.name,
                    "planned operations require prepare()",
                    error,
                )
            })?;
        }
        record_provider_sources(state, &provider.name)?;
    }
    let mut identities = BTreeSet::new();
    for preparation in &preparations {
        if !identities.insert((preparation.provider.as_str(), preparation.identity.as_str())) {
            return Err(WombatError::configuration(format!(
                "provider `{}` planned duplicate operation `{}`",
                preparation.provider, preparation.identity
            )));
        }
    }
    let mut prerequisite_identities = BTreeSet::new();
    for prerequisite in &prerequisites {
        if !prerequisite_identities.insert((
            prerequisite.provider.as_str(),
            prerequisite.identity.as_str(),
        )) {
            return Err(WombatError::configuration(format!(
                "provider `{}` planned duplicate prerequisite `{}`",
                prerequisite.provider, prerequisite.identity
            )));
        }
    }
    let referenced = requirements
        .iter()
        .flat_map(|requirement| {
            requirement
                .binding
                .prerequisites
                .iter()
                .map(move |identity| (requirement.binding.provider.as_str(), identity.as_str()))
        })
        .collect::<BTreeSet<_>>();
    for reference in &referenced {
        if !prerequisite_identities.contains(reference) {
            return Err(WombatError::configuration(format!(
                "provider `{}` binding references absent prerequisite `{}`",
                reference.0, reference.1
            )));
        }
    }
    if let Some(prerequisite) = prerequisites.iter().find(|prerequisite| {
        !referenced.contains(&(
            prerequisite.provider.as_str(),
            prerequisite.identity.as_str(),
        ))
    }) {
        return Err(WombatError::configuration(format!(
            "provider `{}` planned unreferenced prerequisite `{}`",
            prerequisite.provider, prerequisite.identity
        )));
    }
    Ok((prerequisites, preparations))
}

pub(super) fn parse_provider_prerequisite(
    provider: &str,
    value: FrozenValue,
) -> Result<ProviderPrerequisite> {
    let FrozenValue::Map(mut values) = value else {
        return Err(WombatError::configuration(format!(
            "provider `{provider}` plan() entries must be provider.prerequisite() values"
        )));
    };
    let kind = take_string(&mut values, "kind", "provider prerequisite")?;
    if kind != "prerequisite" {
        return Err(WombatError::configuration(format!(
            "provider `{provider}` returned unknown planned value `{kind}`"
        )));
    }
    let identity = take_string(&mut values, "identity", "provider prerequisite")?;
    let description = take_string(&mut values, "description", "provider prerequisite")?;
    if identity.trim().is_empty() || description.trim().is_empty() {
        return Err(WombatError::configuration(
            "provider prerequisite identity and description must not be empty",
        ));
    }
    let elevated = match values.remove("elevated") {
        None => false,
        Some(FrozenValue::Boolean(value)) => value,
        Some(_) => {
            return Err(WombatError::configuration(
                "provider prerequisite `elevated` must be boolean",
            ));
        }
    };
    let data = values.remove("data").unwrap_or_else(FrozenValue::empty_map);
    if !matches!(data, FrozenValue::Map(_)) {
        return Err(WombatError::configuration(
            "provider prerequisite data must be a string-keyed map",
        ));
    }
    reject_unknown_options(&values, "provider prerequisite")?;
    Ok(ProviderPrerequisite {
        provider: provider.to_string(),
        identity,
        description,
        when: crate::execution::ladder::CoreRung::DeployAfter.into(),
        elevated,
        data,
    })
}

pub(super) fn parse_provider_operation(
    provider: &str,
    value: FrozenValue,
) -> Result<ProviderPreparation> {
    let FrozenValue::Map(mut values) = value else {
        return Err(WombatError::configuration(format!(
            "provider `{provider}` plan() entries must be provider.operation() values"
        )));
    };
    let kind = take_string(&mut values, "kind", "provider operation")?;
    if kind != "operation" {
        return Err(WombatError::configuration(format!(
            "provider `{provider}` returned unknown planned value `{kind}`"
        )));
    }
    let identity = take_string(&mut values, "identity", "provider operation")?;
    let description = take_string(&mut values, "description", "provider operation")?;
    if identity.trim().is_empty() || description.trim().is_empty() {
        return Err(WombatError::configuration(
            "provider operation identity and description must not be empty",
        ));
    }
    let elevated = match values.remove("elevated") {
        None => false,
        Some(FrozenValue::Boolean(value)) => value,
        Some(_) => {
            return Err(WombatError::configuration(
                "provider operation `elevated` must be boolean",
            ));
        }
    };
    let data = values.remove("data").unwrap_or_else(FrozenValue::empty_map);
    if !matches!(data, FrozenValue::Map(_)) {
        return Err(WombatError::configuration(
            "provider operation data must be a string-keyed map",
        ));
    }
    reject_unknown_options(&values, "provider operation")?;
    Ok(ProviderPreparation {
        provider: provider.to_string(),
        identity,
        description,
        elevated,
        data,
    })
}

pub(super) fn frozen_binding(binding: &ProviderBinding) -> Result<FrozenValue> {
    Ok(serde_json::from_value(serde_json::to_value(binding)?)?)
}

pub(super) fn install_provider_require(
    lua: &Lua,
    state: Rc<RefCell<RuntimeState>>,
    provider_name: &str,
    api: Table,
) -> Result<()> {
    let provider_name = provider_name.to_string();
    let cache = Rc::new(RefCell::new(BTreeMap::<String, mlua::RegistryKey>::new()));
    let require = lua.create_function(move |lua, module: String| {
        if module == "wombat.provider" {
            return Ok(Value::Table(api.clone()));
        }
        validate_provider_module_name(&module).map_err(mlua::Error::external)?;
        if let Some(key) = cache.borrow().get(&module) {
            return lua.registry_value(key);
        }
        let relative = module.replace('.', "/");
        let root = state.borrow().root.join("providers").join(&provider_name);
        let direct = root.join(format!("{relative}.lua"));
        let initial = root.join(&relative).join("init.lua");
        let path = if direct.is_file() {
            direct
        } else if initial.is_file() {
            initial
        } else {
            return Err(mlua::Error::external(WombatError::configuration(format!(
                "provider `{provider_name}` cannot find helper module `{module}`"
            ))));
        };
        let source = load_tracked_source(&state, &path).map_err(mlua::Error::external)?;
        let name = display_path(&state.borrow().root, &path);
        let value: Value = lua.load(&source).set_name(format!("@{name}")).eval()?;
        let value = if value.is_nil() {
            Value::Boolean(true)
        } else {
            value
        };
        let key = lua.create_registry_value(value.clone())?;
        cache.borrow_mut().insert(module, key);
        Ok(value)
    })?;
    lua.globals().set("require", require)?;
    Ok(())
}

pub(super) fn validate_provider_module_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.split('.').any(|part| {
            part.is_empty()
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
    {
        return Err(WombatError::configuration(format!(
            "invalid provider helper module `{name}`"
        )));
    }
    Ok(())
}

pub(super) fn frozen_candidate(candidate: &RequirementCandidate) -> Result<FrozenValue> {
    Ok(serde_json::from_value(serde_json::to_value(candidate)?)?)
}

pub(super) fn record_provider_sources(
    state: &Rc<RefCell<RuntimeState>>,
    provider_name: &str,
) -> Result<()> {
    let root_prefix = format!("providers/{provider_name}/");
    let entrypoint = format!("providers/{provider_name}.lua");
    let mut files = state
        .borrow()
        .sources
        .values()
        .filter(|source| {
            source.manifest.path == entrypoint || source.manifest.path.starts_with(&root_prefix)
        })
        .map(|source| {
            Ok(crate::model::manifest::ProviderFile {
                source: source.manifest.path.clone(),
                payload: source
                    .manifest
                    .path
                    .strip_prefix("providers/")
                    .expect("provider sources live under providers")
                    .to_string(),
                digest: source.manifest.digest.clone(),
                size: u64::try_from(source.snapshot.len())
                    .map_err(|_| WombatError::configuration("provider source is too large"))?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    files.sort_by(|left, right| left.payload.cmp(&right.payload));
    let mut state = state.borrow_mut();
    let mut found = false;
    if let Some(configured) = state
        .providers
        .iter_mut()
        .find(|provider| provider.name == provider_name)
    {
        found = true;
        if let ProviderOrigin::Custom {
            files: configured_files,
            ..
        } = &mut configured.origin
        {
            *configured_files = files.clone();
        }
    }
    debug_assert!(found, "resolved provider is configured");
    Ok(())
}

pub(super) fn parse_provider_resolution(
    provider: &str,
    value: FrozenValue,
) -> Result<std::result::Result<ProviderBinding, String>> {
    let FrozenValue::Map(mut values) = value else {
        return Err(WombatError::configuration(format!(
            "provider `{provider}` resolve() must return provider.binding() or provider.unsupported()"
        )));
    };
    let kind = take_string(&mut values, "kind", "provider resolution")?;
    if kind == "unsupported" {
        let reason = take_string(&mut values, "reason", "unsupported provider resolution")?;
        reject_unknown_options(&values, "unsupported provider resolution")?;
        return Ok(Err(reason));
    }
    if kind != "binding" {
        return Err(WombatError::configuration(format!(
            "provider `{provider}` returned unknown resolution kind `{kind}`"
        )));
    }
    let identity = take_string(&mut values, "identity", "provider binding")?;
    let elevated = match values.remove("elevated") {
        None => false,
        Some(FrozenValue::Boolean(value)) => value,
        Some(_) => {
            return Err(WombatError::configuration(
                "provider binding `elevated` must be boolean",
            ));
        }
    };
    let package = take_optional_string(&mut values, "package", "provider binding")?;
    let publications = values
        .remove("publications")
        .map(|value| parse_publications("publications", value))
        .transpose()?
        .unwrap_or(Publications {
            commands: Vec::new(),
        });
    let mut prerequisites = match values.remove("prerequisites") {
        None => Vec::new(),
        Some(FrozenValue::Array(values)) => values
            .into_iter()
            .map(|value| match value {
                FrozenValue::String(identity) if !identity.trim().is_empty() => Ok(identity),
                _ => Err(WombatError::configuration(
                    "provider binding prerequisites must be non-empty strings",
                )),
            })
            .collect::<Result<Vec<_>>>()?,
        Some(FrozenValue::Map(values)) if values.is_empty() => Vec::new(),
        Some(_) => {
            return Err(WombatError::configuration(
                "provider binding prerequisites must be an array",
            ));
        }
    };
    prerequisites.sort();
    if prerequisites.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(WombatError::configuration(
            "provider binding prerequisites must be unique",
        ));
    }
    let data = values.remove("data").unwrap_or_else(FrozenValue::empty_map);
    if !matches!(data, FrozenValue::Map(_)) {
        return Err(WombatError::configuration(
            "provider binding data must be a string-keyed map",
        ));
    }
    reject_unknown_options(&values, "provider binding")?;
    Ok(Ok(ProviderBinding {
        provider: provider.to_string(),
        identity,
        elevated,
        package,
        publications,
        prerequisites,
        data,
    }))
}

pub(super) fn provider_lua_error(provider: &str, phase: &str, error: mlua::Error) -> WombatError {
    WombatError::configuration(format!("provider `{provider}` {phase} failed: {error}"))
}

pub(super) fn resolved_requirement_handle(requirement: &Requirement) -> FrozenValue {
    let selected = &requirement.candidates[requirement.selected as usize];
    let mut values = BTreeMap::from([
        (
            "kind".to_string(),
            FrozenValue::String(
                match requirement.kind {
                    RequirementKind::Command => "command",
                    RequirementKind::Package => "package",
                }
                .to_string(),
            ),
        ),
        (
            "name".to_string(),
            FrozenValue::String(selected.name().to_string()),
        ),
        (
            "choice".to_string(),
            FrozenValue::String(
                match requirement.choice {
                    RequirementChoice::Required => "required",
                    RequirementChoice::Preferred => "preferred",
                    RequirementChoice::Accepted => "accepted",
                }
                .to_string(),
            ),
        ),
        (
            "alternative".to_string(),
            FrozenValue::Integer(i64::from(requirement.selected) + 1),
        ),
        (
            "provider".to_string(),
            FrozenValue::String(requirement.binding.provider.clone()),
        ),
        (
            "publications".to_string(),
            FrozenValue::Map(BTreeMap::from([(
                "commands".to_string(),
                FrozenValue::Array(
                    requirement
                        .binding
                        .publications
                        .commands
                        .iter()
                        .cloned()
                        .map(FrozenValue::String)
                        .collect(),
                ),
            )])),
        ),
    ]);
    if let Some(minimum) = selected.minimum() {
        values.insert(
            "minimum".to_string(),
            FrozenValue::String(minimum.to_string()),
        );
    }
    if let Some(package) = &requirement.binding.package {
        values.insert("package".to_string(), FrozenValue::String(package.clone()));
    }
    FrozenValue::Map(values)
}

pub(super) fn take_string(
    options: &mut BTreeMap<String, FrozenValue>,
    key: &str,
    subject: &str,
) -> Result<String> {
    match options.remove(key) {
        Some(FrozenValue::String(value)) if !value.is_empty() => Ok(value),
        _ => Err(WombatError::configuration(format!(
            "{subject} requires a non-empty string `{key}`"
        ))),
    }
}

pub(super) fn take_optional_string(
    options: &mut BTreeMap<String, FrozenValue>,
    key: &str,
    subject: &str,
) -> Result<Option<String>> {
    match options.remove(key) {
        None => Ok(None),
        Some(FrozenValue::String(value)) => Ok(Some(value)),
        Some(_) => Err(WombatError::configuration(format!(
            "{subject} `{key}` must be a string"
        ))),
    }
}

pub(super) fn reject_unknown_options(
    options: &BTreeMap<String, FrozenValue>,
    subject: &str,
) -> Result<()> {
    if let Some(key) = options.keys().next() {
        return Err(WombatError::configuration(format!(
            "{subject} does not support option `{key}`"
        )));
    }
    Ok(())
}

pub(super) fn validate_provider_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || (index > 0 && matches!(byte, b'-' | b'_'))
        })
    {
        return Err(WombatError::configuration(format!(
            "invalid provider name `{name}`; expected lowercase letters, digits, `-`, or `_`"
        )));
    }
    Ok(())
}

pub(super) fn validate_product_name(name: &str, kind: RequirementKind) -> Result<()> {
    let valid = match kind {
        RequirementKind::Command => {
            !name.is_empty()
                && !name.starts_with('-')
                && name.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'+' | b'@')
                })
        }
        RequirementKind::Package => {
            !name.is_empty()
                && !name.starts_with('-')
                && !name
                    .bytes()
                    .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        }
    };
    if !valid {
        return Err(WombatError::configuration(format!(
            "invalid {} requirement name `{name}`",
            match kind {
                RequirementKind::Command => "command",
                RequirementKind::Package => "package",
            }
        )));
    }
    Ok(())
}
