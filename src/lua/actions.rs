//! Artifact, task, script, and ladder declarations exposed to configuration Lua.

use super::*;

pub(super) fn effective_target(state: &RuntimeState) -> TargetPlatform {
    let target = &state.target.platform;
    if target.os.name == state.host.platform.os.name
        && target.arch == state.host.platform.arch
        && target.os.version.is_none()
        && target.os.kernel.is_none()
        && target.os.distribution.is_none()
    {
        state.host.platform.clone()
    } else {
        target.clone()
    }
}

pub(super) fn set_target(
    state: &Rc<RefCell<RuntimeState>>,
    value: Value,
    location: Location,
) -> mlua::Result<()> {
    let frozen = FrozenValue::from_lua(value).map_err(mlua::Error::external)?;
    let mut state = state.borrow_mut();
    if state.active_module().is_some() || state.root_policy_started {
        return Err(mlua::Error::external(WombatError::configuration(format!(
            "w.target() must run in root policy before module selection at {}",
            location.display()
        ))));
    }
    if let Some(prior) = &state.target_override {
        return Err(mlua::Error::external(WombatError::configuration(format!(
            "w.target() may override the target only once; first at {}, repeated at {}",
            prior.display(),
            location.display()
        ))));
    }
    if let Some(prior) = &state.target_first_read {
        return Err(mlua::Error::external(WombatError::configuration(format!(
            "w.target() cannot override a target after it was read; first read at {}, override at {}",
            prior.display(),
            location.display()
        ))));
    }
    let platform = match frozen {
        FrozenValue::String(value) => {
            TargetPlatform::parse_compact(&value).map_err(mlua::Error::external)?
        }
        value => TargetPlatform::from_frozen(&value).map_err(mlua::Error::external)?,
    };
    state.target = ResolvedTarget {
        platform,
        origin: TargetOrigin::RootOverride,
        declared_at: Some(location.trace.clone()),
    };
    state.target_override = Some(location);
    Ok(())
}

pub(super) fn register_selection(
    state: &Rc<RefCell<RuntimeState>>,
    name: &str,
    config: Value,
    location: Location,
) -> Result<()> {
    validate_module_name(name)?;

    let mut state = state.borrow_mut();
    let from = state.active_module().unwrap_or(ROOT_MODULE).to_string();
    let is_module_selection = state.active_module().is_some();
    if !is_module_selection {
        state.root_policy_started = true;
    }
    let explicit_config = if config.is_nil() {
        None
    } else {
        if is_module_selection {
            return Err(WombatError::configuration(format!(
                "module `{from}` cannot configure module `{name}` at {}; configuration-bearing use() belongs to root policy",
                location.display()
            )));
        }
        Some(FrozenValue::from_lua(config)?)
    };

    state.dependencies.insert(Dependency {
        kind: DependencyKind::Use,
        from,
        to: name.to_string(),
        declared_at: location.trace.clone(),
    });

    let record = state
        .modules
        .entry(name.to_string())
        .or_insert_with(ModuleRecord::selected);

    if let Some(value) = explicit_config {
        match &mut record.explicit_config {
            Some(existing) if existing.value == value => existing.locations.push(location),
            Some(existing) => {
                let prior = existing
                    .locations
                    .iter()
                    .map(Location::display)
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(WombatError::configuration(format!(
                    "conflicting configuration for module `{name}`: first selected at {prior}; conflicting selection at {}",
                    location.display()
                )));
            }
            None => {
                record.explicit_config = Some(ExplicitConfig {
                    value,
                    locations: vec![location],
                });
            }
        }
    }

    Ok(())
}

pub(super) fn consume_module(
    lua: &Lua,
    state: &Rc<RefCell<RuntimeState>>,
    name: &str,
    location: Location,
) -> Result<Value> {
    validate_module_name(name)?;

    {
        let mut state = state.borrow_mut();
        let Some(from) = state.active_module().map(str::to_owned) else {
            return Err(WombatError::configuration(
                "using() may only be called while evaluating a Wombat module",
            ));
        };
        if !state.modules.contains_key(name) {
            return Err(WombatError::configuration(format!(
                "module `{from}` uses module `{name}`, but `{name}` was not selected with use()"
            )));
        }
        state.dependencies.insert(Dependency {
            kind: DependencyKind::Using,
            from,
            to: name.to_string(),
            declared_at: location.trace,
        });
    }

    evaluate_module(lua, state, name)?;

    let export = state
        .borrow()
        .modules
        .get(name)
        .and_then(|module| module.export.clone())
        .ok_or_else(|| {
            WombatError::configuration(format!(
                "module `{name}` finished without a resolved public export"
            ))
        })?;
    export.to_lua(lua).map_err(WombatError::from)
}

pub(super) fn current_module_config(lua: &Lua, state: &Rc<RefCell<RuntimeState>>) -> Result<Value> {
    let state = state.borrow();
    let name = state.active_module().ok_or_else(|| {
        WombatError::configuration(
            "module.config() may only be called while evaluating a Wombat module",
        )
    })?;
    let config = state
        .modules
        .get(name)
        .expect("the active module must exist in the registry")
        .config();
    config.to_lua(lua).map_err(WombatError::from)
}

pub(super) fn declare_generated(
    state: &Rc<RefCell<RuntimeState>>,
    name: &str,
    options: Value,
    location: Location,
) -> Result<()> {
    validate_relative_path(name, "generated artifact name")?;
    let Value::Table(options) = options else {
        return Err(WombatError::configuration(
            "w.generate() requires an options table",
        ));
    };
    let keys = options
        .clone()
        .pairs::<Value, Value>()
        .map(|pair| pair.map(|(key, _)| key))
        .collect::<mlua::Result<Vec<_>>>()?;
    for key in keys {
        let Value::String(key) = key else {
            return Err(WombatError::configuration(
                "w.generate() option names must be strings",
            ));
        };
        let key = key.to_str()?;
        if !matches!(key.as_ref(), "content" | "to" | "executable") {
            return Err(WombatError::configuration(format!(
                "w.generate() does not support option `{key}`"
            )));
        }
    }
    let content = options.get::<Value>("content")?;
    let Value::String(content) = content else {
        return Err(WombatError::configuration(
            "w.generate() requires binary-safe string `content`",
        ));
    };
    let bytes = content.as_bytes().to_vec();
    let explicit_target = options.get::<Option<String>>("to")?;
    let executable = options.get::<Option<bool>>("executable")?.unwrap_or(false);

    let mut state = state.borrow_mut();
    let (_, _, module_target, _) = state.active_location();
    let owner = state.active_module().unwrap_or(ROOT_MODULE).to_string();
    let target = match explicit_target.as_deref() {
        Some(target) => {
            let target = parse_explicit_target(target)?;
            if target.scope == crate::model::manifest::TargetScope::Absolute {
                return Err(WombatError::configuration(
                    "w.generate() targets must remain deployment-root-relative; use w.install() for an explicit external file",
                ));
            }
            target
        }
        None => {
            let base = module_target.ok_or_else(|| {
                WombatError::configuration(format!(
                    "cannot infer a target for generated artifact `{name}` from an unallocated module; provide `to` at {}",
                    location.display()
                ))
            })?;
            infer_target(
                &crate::model::path::join_relative(&base, name),
                format!("generated:{name}"),
            )?
        }
    };
    state.root_policy_started = true;
    state.artifacts.push(EvaluatedArtifact {
        kind: ArtifactKind::File,
        source: format!(
            "generated/{}/{}",
            if owner == ROOT_MODULE { "root" } else { &owner },
            name
        ),
        source_origin: SourceOrigin::Generated {
            name: name.to_string(),
        },
        source_projection: None,
        production: EvaluatedProduction::GeneratedLua {
            content: bytes,
            executable,
        },
        target,
        fingerprint: None,
        owner,
        declared_at: location.trace,
    });
    Ok(())
}

pub(super) fn declare_task(
    lua: &Lua,
    state: &Rc<RefCell<RuntimeState>>,
    entrypoint: &str,
    params: Value,
    options: Value,
    location: Location,
) -> Result<()> {
    validate_relative_path(entrypoint, "task entrypoint")?;
    let params = FrozenValue::from_lua(params)?;
    if !matches!(params, FrozenValue::Map(_)) {
        return Err(WombatError::configuration(
            "w.build.task() params must be a string-keyed table",
        ));
    }
    let params_json = serde_json::to_vec(&params)?;
    if params_json.len() > 64 * 1024 {
        return Err(WombatError::configuration(
            "w.build.task() params exceed the 64 KiB argv limit; pass large or binary inputs through files",
        ));
    }
    let frozen = FrozenValue::from_lua(options)?;
    let FrozenValue::Map(mut options) = frozen else {
        return Err(WombatError::configuration(
            "w.build.task() options must be a table",
        ));
    };
    let instance = take_optional_string(&mut options, "name", "task")?;
    if let Some(instance) = &instance
        && (instance.is_empty()
            || !instance
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
    {
        return Err(WombatError::configuration(format!(
            "task instance name `{instance}` is invalid; expected ASCII letters, numbers, `-`, or `_`"
        )));
    }
    let explicit_target = take_optional_string(&mut options, "to", "task")?;
    let python_helper = match options.remove("python_helper") {
        None => true,
        Some(FrozenValue::Boolean(value)) => value,
        Some(_) => {
            return Err(WombatError::configuration(
                "task `python_helper` must be boolean",
            ));
        }
    };
    let logs = match options.remove("logs") {
        None => TaskLogPolicy::Failure,
        Some(FrozenValue::String(value)) if value == "failure" => TaskLogPolicy::Failure,
        Some(FrozenValue::String(value)) if value == "always" => TaskLogPolicy::Always,
        Some(FrozenValue::String(value)) if value == "never" => TaskLogPolicy::Never,
        Some(_) => {
            return Err(WombatError::configuration(
                "task `logs` must be `failure`, `always`, or `never`",
            ));
        }
    };
    let cache = match options.remove("cache") {
        None | Some(FrozenValue::Boolean(true)) => TaskCachePolicy {
            enabled: true,
            revision: None,
        },
        Some(FrozenValue::Boolean(false)) => TaskCachePolicy {
            enabled: false,
            revision: None,
        },
        Some(FrozenValue::Map(mut cache)) => {
            let revision = take_optional_string(&mut cache, "revision", "task cache")?;
            reject_unknown_options(&cache, "task cache")?;
            TaskCachePolicy {
                enabled: true,
                revision,
            }
        }
        Some(_) => {
            return Err(WombatError::configuration(
                "task `cache` must be boolean or a table containing optional `revision`",
            ));
        }
    };
    let at = match options.remove("at") {
        None => CoreRung::MaterialiseTasks.into(),
        Some(FrozenValue::String(value)) => RungId::new(value)?,
        Some(_) => {
            return Err(WombatError::configuration(
                "task `at` must be a w.rungs handle or canonical rung string",
            ));
        }
    };

    let root = state.borrow().root.clone();
    let absolute = root.join("tasks").join(entrypoint);
    validate_source_components(&root, &absolute)?;
    let fingerprint = fingerprint_regular_file(&absolute).map_err(|error| {
        error.with_note(format!(
            "task `{entrypoint}` must be a regular file beneath `tasks/`"
        ))
    })?;
    let bytes = fs::read(&absolute).map_err(|error| WombatError::io(&absolute, error))?;
    let entrypoint_digest = digest_bytes(&bytes);
    let runner = parse_task_runner(
        entrypoint,
        options.remove("interpreter"),
        &absolute,
        &state.borrow().task_interpreters,
    )?;
    reject_unknown_options(&options, "task")?;

    if let Some(command) = runner.command()
        && !command.contains('/')
        && !command.contains('\\')
        && !state.borrow().providers.is_empty()
    {
        let requirement_options = lua.create_table()?;
        requirement_options.set("when", at.id())?;
        let _ = declare_requirement(
            lua,
            state,
            RequirementDeclaration {
                kind: "command",
                name: command,
                options: Value::Table(requirement_options),
                preferred: false,
                location: location.clone(),
            },
        )?;
    }

    let mut state = state.borrow_mut();
    let (_, _, module_target, _) = state.active_location();
    let owner = state.active_module().unwrap_or(ROOT_MODULE).to_string();
    let identity = format!(
        "{}:{}{}",
        owner,
        entrypoint,
        instance
            .as_ref()
            .map(|value| format!("#{value}"))
            .unwrap_or_default()
    );
    if state
        .tasks
        .iter()
        .any(|task| task.task.identity == identity)
    {
        return Err(WombatError::configuration(format!(
            "task `{identity}` is declared more than once; add a unique `name` option"
        )));
    }
    let target_root = match explicit_target.as_deref() {
        Some(target) => Some(parse_explicit_target_root(target)?),
        None => module_target
            .map(|base| infer_target_root(&base, format!("module:{owner}")))
            .transpose()?,
    }
    .map(|root| TaskTargetRoot {
        path: root.path,
        origin: root.origin,
    });
    let declaration_order = state.next_action_order;
    state.next_action_order += 1;
    state.root_policy_started = true;
    state.tasks.push(EvaluatedTask {
        task: Task {
            identity,
            declaration_order,
            owner,
            entrypoint: format!("tasks/{entrypoint}"),
            entrypoint_digest,
            params,
            runner,
            python_helper,
            logs,
            cache,
            at,
            target_root,
            declared_at: location.trace,
            outputs: Vec::new(),
        },
        fingerprint,
    });
    Ok(())
}

pub(super) fn declare_ladder(
    state: &Rc<RefCell<RuntimeState>>,
    name: &str,
    rungs: Value,
    location: Location,
) -> Result<()> {
    let frozen = FrozenValue::from_lua(rungs)?;
    let roots = parse_ladder_rungs(frozen, "ladder")?;
    let ladder = ExecutionLadder::new(name.to_string(), roots)
        .map_err(|error| error.with_note(format!("declared at {}", location.display())))?;
    let mut state = state.borrow_mut();
    if state.active_module().is_some() {
        return Err(WombatError::configuration(format!(
            "w.ladder() may only be called from root configuration at {}",
            location.display()
        )));
    }
    if state.ladder.is_some() {
        return Err(WombatError::configuration(format!(
            "root configuration selects more than one ladder at {}",
            location.display()
        )));
    }
    state.root_policy_started = true;
    state.ladder = Some(ladder);
    Ok(())
}

pub(super) fn parse_ladder_rungs(value: FrozenValue, subject: &str) -> Result<Vec<LadderRung>> {
    let values = match value {
        FrozenValue::Array(values) => values,
        FrozenValue::Map(values) if values.is_empty() => Vec::new(),
        _ => {
            return Err(WombatError::configuration(format!(
                "{subject} rungs must be an array"
            )));
        }
    };
    values
        .into_iter()
        .map(|value| {
            let FrozenValue::Map(mut node) = value else {
                return Err(WombatError::configuration(format!(
                    "{subject} entries must be rung values"
                )));
            };
            let id = RungId::new(take_string(&mut node, "id", "ladder rung")?)?;
            let children = parse_ladder_rungs(
                node.remove("children")
                    .unwrap_or_else(|| FrozenValue::Array(Vec::new())),
                "nested ladder",
            )?;
            reject_unknown_options(&node, "ladder rung")?;
            Ok(LadderRung { id, children })
        })
        .collect()
}

pub(super) fn declare_script(
    lua: &Lua,
    state: &Rc<RefCell<RuntimeState>>,
    entrypoint: &str,
    params: Value,
    options: Value,
    location: Location,
) -> Result<()> {
    validate_relative_path(entrypoint, "script entrypoint")?;
    let params = FrozenValue::from_lua(params)?;
    if !matches!(params, FrozenValue::Map(_)) {
        return Err(WombatError::configuration(
            "w.script() params must be a string-keyed table",
        ));
    }
    if serde_json::to_vec(&params)?.len() > 64 * 1024 {
        return Err(WombatError::configuration(
            "w.script() params exceed the 64 KiB argv limit; pass large or binary inputs through files",
        ));
    }
    let FrozenValue::Map(mut options) = FrozenValue::from_lua(options)? else {
        return Err(WombatError::configuration(
            "w.script() options must be a table",
        ));
    };
    let name = take_optional_string(&mut options, "name", "script")?;
    if let Some(name) = &name
        && (name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
    {
        return Err(WombatError::configuration(format!(
            "script name `{name}` is invalid; expected ASCII letters, numbers, `-`, or `_`"
        )));
    }
    let at = match options.remove("at") {
        None => CoreRung::MaterialiseBefore.into(),
        Some(FrozenValue::String(value)) => RungId::new(value)?,
        Some(_) => {
            return Err(WombatError::configuration(
                "script `at` must be a rung handle or canonical rung string",
            ));
        }
    };
    let schedule = match options.remove("schedule") {
        None => ScriptSchedule::Always,
        Some(FrozenValue::String(value)) if value == "always" => ScriptSchedule::Always,
        Some(FrozenValue::String(value)) if value == "once" => ScriptSchedule::Once,
        Some(FrozenValue::String(value)) if value == "onchange" => ScriptSchedule::Onchange,
        Some(_) => {
            return Err(WombatError::configuration(
                "script `schedule` must be `always`, `once`, or `onchange`",
            ));
        }
    };
    let scope = match options.remove("scope") {
        None => ScriptScope::Target,
        Some(FrozenValue::String(value)) if value == "target" => ScriptScope::Target,
        Some(FrozenValue::String(value)) if value == "host" => ScriptScope::Host,
        Some(_) => {
            return Err(WombatError::configuration(
                "script `scope` must be `target` or `host`",
            ));
        }
    };
    let python_helper = match options.remove("python_helper") {
        None | Some(FrozenValue::Boolean(true)) => true,
        Some(FrozenValue::Boolean(false)) => false,
        Some(_) => {
            return Err(WombatError::configuration(
                "script `python_helper` must be boolean",
            ));
        }
    };
    let logs = match options.remove("logs") {
        None => TaskLogPolicy::Failure,
        Some(FrozenValue::String(value)) if value == "failure" => TaskLogPolicy::Failure,
        Some(FrozenValue::String(value)) if value == "always" => TaskLogPolicy::Always,
        Some(FrozenValue::String(value)) if value == "never" => TaskLogPolicy::Never,
        Some(_) => {
            return Err(WombatError::configuration(
                "script `logs` must be `failure`, `always`, or `never`",
            ));
        }
    };
    let files = match options.remove("files") {
        None => Vec::new(),
        Some(FrozenValue::Array(values)) => values
            .into_iter()
            .map(|value| match value {
                FrozenValue::String(value) => Ok(value),
                _ => Err(WombatError::configuration(
                    "script `files` entries must be strings",
                )),
            })
            .collect::<Result<Vec<_>>>()?,
        Some(_) => {
            return Err(WombatError::configuration(
                "script `files` must be an array",
            ));
        }
    };
    let revision = take_optional_string(&mut options, "revision", "script")?;
    let env = match options.remove("env") {
        None => BTreeMap::new(),
        Some(FrozenValue::Map(values)) => values
            .into_iter()
            .map(|(key, value)| match value {
                FrozenValue::String(value) if !key.contains('=') && !key.contains('\0') => {
                    Ok((key, value))
                }
                _ => Err(WombatError::configuration(
                    "script `env` must map valid names to strings",
                )),
            })
            .collect::<Result<BTreeMap<_, _>>>()?,
        Some(_) => {
            return Err(WombatError::configuration(
                "script `env` must be a string map",
            ));
        }
    };
    let timeout_seconds = match options.remove("timeout") {
        None => None,
        Some(FrozenValue::Integer(value)) if value > 0 => Some(
            u64::try_from(value)
                .map_err(|_| WombatError::configuration("script timeout is too large"))?,
        ),
        Some(_) => {
            return Err(WombatError::configuration(
                "script `timeout` must be a positive integer number of seconds",
            ));
        }
    };

    let root = state.borrow().root.clone();
    let scripts_root = root.join("scripts");
    let absolute = scripts_root.join(entrypoint);
    validate_source_components(&root, &absolute)?;
    fingerprint_regular_file(&absolute).map_err(|error| {
        error.with_note(format!(
            "script `{entrypoint}` must be a regular file beneath `scripts/`"
        ))
    })?;
    let runner = parse_task_runner(
        entrypoint,
        options.remove("interpreter"),
        &absolute,
        &state.borrow().task_interpreters,
    )?;
    reject_unknown_options(&options, "script")?;
    let payloads = collect_script_payloads(&root, entrypoint, &files)?;

    if let Some(command) = runner.command()
        && !command.contains('/')
        && !command.contains('\\')
        && !state.borrow().providers.is_empty()
    {
        let requirement_options = lua.create_table()?;
        requirement_options.set("when", at.id())?;
        let _ = declare_requirement(
            lua,
            state,
            RequirementDeclaration {
                kind: "command",
                name: command,
                options: Value::Table(requirement_options),
                preferred: false,
                location: location.clone(),
            },
        )?;
    }

    let mut state = state.borrow_mut();
    let owner = state.active_module().unwrap_or(ROOT_MODULE).to_string();
    let identity = format!(
        "{}:{}{}:{}:{}",
        owner,
        entrypoint,
        name.as_ref()
            .map(|value| format!("#{value}"))
            .unwrap_or_default(),
        match scope {
            ScriptScope::Target => "target",
            ScriptScope::Host => "host",
        },
        at.id(),
    );
    if state
        .scripts
        .iter()
        .any(|script| script.identity == identity)
    {
        return Err(WombatError::configuration(format!(
            "script `{identity}` is declared more than once; add a unique `name` option"
        )));
    }
    let declaration_order = state.next_action_order;
    state.next_action_order += 1;
    state.root_policy_started = true;
    state.scripts.push(Script {
        identity,
        declaration_order,
        owner,
        entrypoint: format!("scripts/{entrypoint}"),
        params,
        runner,
        python_helper,
        logs,
        at,
        schedule,
        scope,
        payloads,
        revision,
        env,
        timeout_seconds,
        declared_at: location.trace,
    });
    Ok(())
}

pub(super) fn collect_script_payloads(
    root: &Path,
    entrypoint: &str,
    patterns: &[String],
) -> Result<Vec<ScriptPayload>> {
    const MAX_SCRIPT_FILES: usize = 4_096;
    const MAX_SCRIPT_FILE_SIZE: u64 = 16 * 1024 * 1024;
    let scripts_root = root.join("scripts");
    let mut selected = BTreeSet::from([entrypoint.to_string()]);
    if !patterns.is_empty() {
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            validate_relative_path(pattern, "script companion pattern")?;
            builder.add(Glob::new(pattern).map_err(|error| {
                WombatError::configuration(format!(
                    "invalid script companion glob `{pattern}`: {error}"
                ))
            })?);
        }
        let matcher = builder.build().map_err(|error| {
            WombatError::configuration(format!("invalid script companion globs: {error}"))
        })?;
        let mut pending = vec![scripts_root.clone()];
        while let Some(directory) = pending.pop() {
            let mut entries = fs::read_dir(&directory)
                .map_err(|error| WombatError::io(&directory, error))?
                .collect::<std::io::Result<Vec<_>>>()
                .map_err(|error| WombatError::io(&directory, error))?;
            entries.sort_by_key(std::fs::DirEntry::file_name);
            for entry in entries {
                let path = entry.path();
                let metadata =
                    fs::symlink_metadata(&path).map_err(|error| WombatError::io(&path, error))?;
                if metadata.file_type().is_symlink() {
                    return Err(WombatError::configuration(format!(
                        "script payload `{}` must not be a symbolic link",
                        path.display()
                    )));
                }
                if metadata.is_dir() {
                    pending.push(path);
                } else if metadata.is_file() {
                    let relative = path
                        .strip_prefix(&scripts_root)
                        .expect("walk remains in scripts")
                        .to_string_lossy()
                        .replace('\\', "/");
                    if matcher.is_match(&relative) {
                        selected.insert(relative);
                    }
                }
            }
        }
    }
    if selected.len() > MAX_SCRIPT_FILES {
        return Err(WombatError::configuration(
            "script payload exceeds 4096 files",
        ));
    }
    selected
        .into_iter()
        .map(|relative| {
            let path = scripts_root.join(&relative);
            validate_source_components(root, &path)?;
            let metadata = fs::metadata(&path).map_err(|error| WombatError::io(&path, error))?;
            if !metadata.is_file() || metadata.len() > MAX_SCRIPT_FILE_SIZE {
                return Err(WombatError::configuration(format!(
                    "script payload `{relative}` must be a regular file no larger than 16 MiB"
                )));
            }
            let bytes = fs::read(&path).map_err(|error| WombatError::io(&path, error))?;
            Ok(ScriptPayload {
                source: format!("scripts/{relative}"),
                relative,
                digest: digest_bytes(&bytes),
                size: metadata.len(),
                executable: source_executable(&path)?,
            })
        })
        .collect()
}

pub(super) fn parse_task_runner(
    entrypoint: &str,
    configured: Option<FrozenValue>,
    absolute: &Path,
    configured_defaults: &BTreeMap<String, TaskRunner>,
) -> Result<TaskRunner> {
    if let Some(configured) = configured {
        let (command, args) = match configured {
            FrozenValue::String(command) if !command.is_empty() => (command, Vec::new()),
            FrozenValue::Map(mut values) => {
                let command = take_string(&mut values, "command", "task interpreter")?;
                let args = match values.remove("args") {
                    None => Vec::new(),
                    Some(FrozenValue::Array(values)) => values
                        .into_iter()
                        .map(|value| match value {
                            FrozenValue::String(value) => Ok(value),
                            _ => Err(WombatError::configuration(
                                "task interpreter args must be strings",
                            )),
                        })
                        .collect::<Result<Vec<_>>>()?,
                    Some(_) => {
                        return Err(WombatError::configuration(
                            "task interpreter `args` must be an array",
                        ));
                    }
                };
                reject_unknown_options(&values, "task interpreter")?;
                (command, args)
            }
            _ => {
                return Err(WombatError::configuration(
                    "task `interpreter` must be a non-empty command string or table",
                ));
            }
        };
        validate_interpreter_command(&command)?;
        return Ok(TaskRunner::Interpreter {
            contract_version: 1,
            family: if entrypoint.ends_with(".py") {
                InterpreterFamily::Python
            } else {
                InterpreterFamily::Custom
            },
            command,
            args,
        });
    }
    let configured_name = match Path::new(entrypoint)
        .extension()
        .and_then(|extension| extension.to_str())
    {
        Some("py") => Some("python"),
        Some("sh") => Some("shell"),
        Some("bash") => Some("bash"),
        Some("lua") => Some("lua"),
        _ => None,
    };
    if let Some(runner) = configured_name.and_then(|name| configured_defaults.get(name)) {
        return Ok(runner.clone());
    }
    let runner = match Path::new(entrypoint)
        .extension()
        .and_then(|extension| extension.to_str())
    {
        Some("py") => TaskRunner::Interpreter {
            contract_version: 1,
            family: InterpreterFamily::Python,
            command: "python3".to_string(),
            args: Vec::new(),
        },
        Some("sh") => TaskRunner::Interpreter {
            contract_version: 1,
            family: InterpreterFamily::PosixShell,
            command: "sh".to_string(),
            args: Vec::new(),
        },
        Some("bash") => TaskRunner::Interpreter {
            contract_version: 1,
            family: InterpreterFamily::Bash,
            command: "bash".to_string(),
            args: Vec::new(),
        },
        Some("lua") => TaskRunner::EmbeddedLua {
            contract_version: 1,
        },
        None if source_executable(absolute)? => TaskRunner::Direct {
            contract_version: 1,
        },
        None => {
            return Err(WombatError::configuration(format!(
                "extensionless task `{entrypoint}` must be executable or declare `interpreter`"
            )));
        }
        Some(extension) => {
            return Err(WombatError::configuration(format!(
                "cannot infer an interpreter for task `{entrypoint}` with extension `.{extension}`; declare `interpreter`"
            )));
        }
    };
    Ok(runner)
}

pub(super) fn validate_interpreter_command(command: &str) -> Result<()> {
    if command.is_empty() {
        return Err(WombatError::configuration(
            "task interpreter command must not be empty",
        ));
    }
    let path = Path::new(command);
    if path.components().count() > 1 && !path.is_absolute() {
        return Err(WombatError::configuration(format!(
            "task interpreter `{command}` must be a bare command or absolute path"
        )));
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn source_executable(path: &Path) -> Result<bool> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = fs::metadata(path).map_err(|error| WombatError::io(path, error))?;
    Ok(metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
pub(super) fn source_executable(_path: &Path) -> Result<bool> {
    Ok(false)
}
