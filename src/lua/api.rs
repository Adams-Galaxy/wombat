//! Native Lua module construction, logging, data loading, and process observations.

use super::*;

pub(super) fn create_native_module(lua: &Lua, state: Rc<RefCell<RuntimeState>>) -> Result<Table> {
    let native = lua.create_table()?;

    let spec_state = Rc::clone(&state);
    native.set(
        "input_spec",
        lua.create_function(move |lua, (kind, options): (String, Value)| {
            let location = caller_location(lua, &spec_state);
            register_input_spec(&spec_state, &kind, options, location)
                .map_err(mlua::Error::external)
        })?,
    )?;

    let inputs_state = Rc::clone(&state);
    native.set(
        "resolve_inputs",
        lua.create_function(move |lua, schema: Table| {
            let location = caller_location(lua, &inputs_state);
            resolve_inputs(lua, &inputs_state, schema, location).map_err(mlua::Error::external)
        })?,
    )?;

    let host_state = Rc::clone(&state);
    native.set(
        "host_context",
        lua.create_function(move |lua, ()| {
            create_context_proxy(
                lua,
                Rc::clone(&host_state),
                ObservationSubject::Host,
                String::new(),
                false,
            )
        })?,
    )?;

    let target_state = Rc::clone(&state);
    native.set(
        "target_context",
        lua.create_function(move |lua, ()| {
            create_context_proxy(
                lua,
                Rc::clone(&target_state),
                ObservationSubject::Target,
                String::new(),
                true,
            )
        })?,
    )?;

    let common_os_state = Rc::clone(&state);
    native.set(
        "common_os_context",
        lua.create_function(move |lua, ()| {
            create_common_os_proxy(lua, Rc::clone(&common_os_state))
        })?,
    )?;

    let common_value_state = Rc::clone(&state);
    native.set(
        "common_value",
        lua.create_function(move |lua, name: String| {
            let location = caller_location(lua, &common_value_state);
            common_value(lua, &common_value_state, &name, location).map_err(mlua::Error::external)
        })?,
    )?;

    let paths_state = Rc::clone(&state);
    native.set(
        "paths_context",
        lua.create_function(move |lua, ()| create_paths_proxy(lua, Rc::clone(&paths_state)))?,
    )?;

    let use_state = Rc::clone(&state);
    native.set(
        "use_module",
        lua.create_function(move |lua, (name, config): (String, Value)| {
            let location = caller_location(lua, &use_state);
            register_selection(&use_state, &name, config, location).map_err(mlua::Error::external)
        })?,
    )?;

    let using_state = Rc::clone(&state);
    native.set(
        "using_module",
        lua.create_function(move |lua, name: String| {
            let location = caller_location(lua, &using_state);
            consume_module(lua, &using_state, &name, location).map_err(mlua::Error::external)
        })?,
    )?;

    let config_state = Rc::clone(&state);
    native.set(
        "module_config",
        lua.create_function(move |lua, ()| {
            current_module_config(lua, &config_state).map_err(mlua::Error::external)
        })?,
    )?;

    let providers_state = Rc::clone(&state);
    native.set(
        "configure_providers",
        lua.create_function(move |lua, entries: Value| {
            let location = caller_location(lua, &providers_state);
            configure_providers(&providers_state, entries, location).map_err(mlua::Error::external)
        })?,
    )?;

    let requirement_state = Rc::clone(&state);
    native.set(
        "declare_requirement",
        lua.create_function(
            move |lua, (kind, name, options, preferred): (String, String, Value, bool)| {
                let location = caller_location(lua, &requirement_state);
                declare_requirement(
                    lua,
                    &requirement_state,
                    RequirementDeclaration {
                        kind: &kind,
                        name: &name,
                        options,
                        preferred,
                        location,
                    },
                )
                .map_err(mlua::Error::external)
            },
        )?,
    )?;

    let task_state = Rc::clone(&state);
    native.set(
        "declare_task",
        lua.create_function(
            move |lua, (entrypoint, params, options): (String, Value, Value)| {
                let location = caller_location(lua, &task_state);
                declare_task(lua, &task_state, &entrypoint, params, options, location)
                    .map_err(mlua::Error::external)
            },
        )?,
    )?;

    let script_state = Rc::clone(&state);
    native.set(
        "declare_script",
        lua.create_function(
            move |lua, (entrypoint, params, options): (String, Value, Value)| {
                let location = caller_location(lua, &script_state);
                declare_script(lua, &script_state, &entrypoint, params, options, location)
                    .map_err(mlua::Error::external)
            },
        )?,
    )?;

    let ladder_state = Rc::clone(&state);
    native.set(
        "declare_ladder",
        lua.create_function(move |lua, (name, rungs): (String, Value)| {
            let location = caller_location(lua, &ladder_state);
            declare_ladder(&ladder_state, &name, rungs, location).map_err(mlua::Error::external)
        })?,
    )?;

    let generated_state = Rc::clone(&state);
    native.set(
        "declare_generated",
        lua.create_function(move |lua, (name, options): (String, Value)| {
            let location = caller_location(lua, &generated_state);
            declare_generated(&generated_state, &name, options, location)
                .map_err(mlua::Error::external)
        })?,
    )?;

    let module_from_state = Rc::clone(&state);
    native.set(
        "module_from",
        lua.create_function(move |lua, (source, target): (Value, Option<String>)| {
            let location = caller_location(lua, &module_from_state);
            declare_module_from(&module_from_state, source, target.as_deref(), location)
                .map_err(mlua::Error::external)
        })?,
    )?;

    native.set(
        "hidden_source",
        lua.create_function(|lua, source: String| {
            let value = lua.create_table()?;
            value.set("__wombat_hidden", source)?;
            Ok(value)
        })?,
    )?;

    let toml_decode_state = Rc::clone(&state);
    native.set(
        "toml_decode",
        lua.create_function(move |lua, path: String| {
            let location = caller_location(lua, &toml_decode_state);
            read_toml_data(lua, &toml_decode_state, &path, location).map_err(mlua::Error::external)
        })?,
    )?;

    let json_decode_state = Rc::clone(&state);
    native.set(
        "json_decode",
        lua.create_function(move |lua, path: String| {
            let location = caller_location(lua, &json_decode_state);
            read_json_data(lua, &json_decode_state, &path, location).map_err(mlua::Error::external)
        })?,
    )?;

    let yaml_decode_state = Rc::clone(&state);
    native.set(
        "yaml_decode",
        lua.create_function(move |lua, path: String| {
            let location = caller_location(lua, &yaml_decode_state);
            read_yaml_data(lua, &yaml_decode_state, &path, location).map_err(mlua::Error::external)
        })?,
    )?;

    native.set(
        "toml_encode",
        lua.create_function(|_, value: Value| {
            encode_toml_data(value).map_err(mlua::Error::external)
        })?,
    )?;

    native.set(
        "json_encode",
        lua.create_function(|_, value: Value| {
            encode_json_data(value).map_err(mlua::Error::external)
        })?,
    )?;

    native.set(
        "yaml_encode",
        lua.create_function(|_, value: Value| {
            encode_yaml_data(value).map_err(mlua::Error::external)
        })?,
    )?;

    native.set("null", Value::NULL)?;
    native.set(
        "array",
        lua.create_function(|lua, value: Option<Table>| {
            let table = value.map_or_else(|| lua.create_table(), Ok)?;
            match FrozenValue::from_lua(Value::Table(table.clone()))
                .map_err(mlua::Error::external)?
            {
                FrozenValue::Array(_) => {}
                FrozenValue::Map(values) if values.is_empty() => {}
                _ => {
                    return Err(mlua::Error::external(WombatError::configuration(
                        "w.array() requires a contiguous positive-integer-keyed table",
                    )));
                }
            }
            crate::model::frozen::mark_lua_array(lua, &table)?;
            Ok(table)
        })?,
    )?;

    native.set(
        "declare_template_helpers",
        super::template_helpers::register_native(lua, Rc::clone(&state))?,
    )?;

    let template_context_state = Rc::clone(&state);
    native.set(
        "template_context",
        lua.create_function(move |lua, value: Value| {
            let location = caller_location(lua, &template_context_state);
            resolve_template_context(lua, &template_context_state, value, location)
                .map_err(mlua::Error::external)
        })?,
    )?;

    let log_state = Rc::clone(&state);
    native.set(
        "log",
        lua.create_function(
            move |lua, (level, message, fields): (String, String, Value)| {
                let location = caller_location(lua, &log_state);
                emit_lua_log(lua, &log_state, &level, &message, fields, location)
                    .map_err(mlua::Error::external)
            },
        )?,
    )?;

    let exec_state = Rc::clone(&state);
    native.set(
        "exec",
        lua.create_function(move |lua, (argv, options): (Value, Value)| {
            let location = caller_location(lua, &exec_state);
            run_observed_process(lua, &exec_state, argv, options, None, location)
                .map_err(mlua::Error::external)
        })?,
    )?;
    let shell_state = Rc::clone(&state);
    native.set(
        "shell",
        lua.create_function(move |lua, (command, options): (String, Value)| {
            let location = caller_location(lua, &shell_state);
            run_observed_process(
                lua,
                &shell_state,
                Value::String(lua.create_string(command)?),
                options,
                Some(()),
                location,
            )
            .map_err(mlua::Error::external)
        })?,
    )?;

    native.set(
        "install_path",
        lua.create_function(
            move |lua,
                  (source, target, kind, context, exclusions, allow_empty): (
                Value,
                Option<String>,
                String,
                Value,
                Vec<String>,
                bool,
            )| {
                let location = caller_location(lua, &state);
                let (source_path, hidden) = decode_source_selector(source)?;
                register_artifact(
                    &state,
                    ArtifactDeclaration {
                        source_path: &source_path,
                        hidden,
                        explicit_target: target.as_deref(),
                        requested_kind: &kind,
                        context,
                        exclusions,
                        allow_empty,
                        location,
                    },
                )
                .map_err(mlua::Error::external)
            },
        )?,
    )?;

    Ok(native)
}

pub(super) fn emit_lua_log(
    _lua: &Lua,
    state: &Rc<RefCell<RuntimeState>>,
    level: &str,
    message: &str,
    fields: Value,
    location: Location,
) -> Result<()> {
    let Some(level) = crate::presentation::LogLevel::parse(level) else {
        return Err(WombatError::configuration(format!(
            "unknown log level `{level}`"
        )));
    };
    if level < state.borrow().log_level {
        return Ok(());
    }
    let fields = if matches!(fields, Value::Nil) {
        String::new()
    } else {
        format!(
            " {}",
            serde_json::to_string(&FrozenValue::from_lua(fields)?)?
        )
    };
    crate::presentation::emit(crate::presentation::Event::Log {
        level,
        message: format!("{message}{fields} ({})", location.display()),
    });
    Ok(())
}

const DEFAULT_MAX_PROCESS_OUTPUT: u64 = 1024 * 1024;
const MAX_PROCESS_OUTPUT: u64 = 64 * 1024 * 1024;

struct ProcessOptions {
    cwd: PathBuf,
    cwd_display: String,
    environment: Vec<ProcessEnvironmentChange>,
    stdin: Option<Vec<u8>>,
    timeout_ms: Option<u64>,
    max_output: u64,
    sensitive: bool,
    shell: Option<String>,
}

pub(super) fn run_observed_process(
    lua: &Lua,
    state: &Rc<RefCell<RuntimeState>>,
    request: Value,
    options: Value,
    shell_request: Option<()>,
    location: Location,
) -> Result<Value> {
    let root = state.borrow().root.clone();
    let options = parse_process_options(&root, options, location.clone())?;
    let (mut command, invocation) = if shell_request.is_some() {
        let Value::String(command) = request else {
            unreachable!("shell request is a string")
        };
        let command = command.to_str()?.to_string();
        reject_nul(&command, "shell command")?;
        let shell = options
            .shell
            .clone()
            .unwrap_or_else(|| "/bin/sh".to_string());
        let mut process = Command::new(&shell);
        process.arg("-c").arg(&command);
        (process, ProcessInvocation::Shell { command, shell })
    } else {
        let argv = lua_string_array(request, "w.exec() argv")?;
        if argv.is_empty() {
            return Err(WombatError::configuration(
                "w.exec() argv must not be empty",
            ));
        }
        let mut process = Command::new(&argv[0]);
        process.args(&argv[1..]);
        (process, ProcessInvocation::Exec { argv })
    };
    command.current_dir(&options.cwd);
    for change in &options.environment {
        match &change.value {
            Some(value) => {
                command.env(&change.name, value);
            }
            None => {
                command.env_remove(&change.name);
            }
        }
    }
    let output_limit = usize::try_from(options.max_output)
        .map_err(|_| WombatError::configuration("process output limit exceeds usize"))?;
    let outcome = crate::execution::process::run(
        &mut command,
        "construction",
        options.timeout_ms.map(Duration::from_millis),
        output_limit,
        options.stdin.as_deref(),
        crate::execution::process::Forwarding::Retained,
    )?;
    if outcome.timed_out {
        return Err(WombatError::process(format!(
            "construction process timed out after {} ms at {}",
            options.timeout_ms.unwrap_or_default(),
            location.display()
        )));
    }
    if outcome.stdout.truncated || outcome.stderr.truncated {
        return Err(WombatError::process(format!(
            "construction process output exceeded the {} byte limit at {}",
            options.max_output,
            location.display()
        )));
    }
    let stdout = outcome.stdout.bytes;
    let stderr = outcome.stderr.bytes;
    let stdout_size = u64::try_from(stdout.len())
        .map_err(|_| WombatError::configuration("process stdout exceeds u64"))?;
    let stderr_size = u64::try_from(stderr.len())
        .map_err(|_| WombatError::configuration("process stderr exceeds u64"))?;
    let code = outcome.code;
    let signal = outcome.signal;
    let observation = ProcessObservation {
        invocation,
        cwd: options.cwd_display,
        environment: options.environment,
        stdin_digest: options.stdin.as_ref().map(|value| digest_bytes(value)),
        timeout_ms: options.timeout_ms,
        max_output: options.max_output,
        sensitive: options.sensitive,
        ok: outcome.success,
        code,
        signal,
        stdout_size,
        stdout_digest: digest_bytes(&stdout),
        stderr_size,
        stderr_digest: digest_bytes(&stderr),
        declared_at: location.trace.clone(),
    };
    state.borrow_mut().process_observations.push(observation);
    process_result(
        lua,
        outcome.success,
        code,
        signal,
        &stdout,
        &stderr,
        location,
    )
}

fn parse_process_options(root: &Path, value: Value, location: Location) -> Result<ProcessOptions> {
    let table = match value {
        Value::Nil => None,
        Value::Table(table) => Some(table),
        _ => {
            return Err(WombatError::configuration(
                "process options must be a table",
            ));
        }
    };
    let mut seen = std::collections::BTreeSet::new();
    let mut cwd = root.to_path_buf();
    let mut cwd_display = ".".to_string();
    let mut environment = Vec::new();
    let mut stdin = None;
    let mut timeout_ms = None;
    let mut max_output = DEFAULT_MAX_PROCESS_OUTPUT;
    let mut sensitive = false;
    let mut shell = None;
    if let Some(table) = table {
        for pair in table.pairs::<String, Value>() {
            let (key, value) = pair?;
            seen.insert(key.clone());
            match key.as_str() {
                "cwd" => {
                    let Value::String(path) = value else {
                        return Err(WombatError::configuration("process `cwd` must be a string"));
                    };
                    let path = path.to_str()?.to_string();
                    reject_nul(&path, "cwd")?;
                    let candidate = PathBuf::from(&path);
                    if candidate.is_absolute() {
                        cwd = candidate;
                        cwd_display = path;
                    } else {
                        if candidate
                            .components()
                            .any(|part| !matches!(part, Component::Normal(_)))
                        {
                            return Err(WombatError::configuration(format!(
                                "process cwd `{path}` escapes the repository at {}",
                                location.display()
                            )));
                        }
                        cwd = root.join(&candidate);
                        cwd_display = path;
                    }
                }
                "env" => {
                    let Value::Table(values) = value else {
                        return Err(WombatError::configuration(
                            "process `env` must be a string-keyed table",
                        ));
                    };
                    for pair in values.pairs::<String, Value>() {
                        let (name, value) = pair?;
                        if name.is_empty() || name.contains('=') || name.contains('\0') {
                            return Err(WombatError::configuration(
                                "process environment name is invalid",
                            ));
                        }
                        let value = match value {
                            Value::Boolean(false) => None,
                            Value::String(value) => {
                                let value = value.to_str()?.to_string();
                                reject_nul(&value, "environment value")?;
                                Some(value)
                            }
                            _ => {
                                return Err(WombatError::configuration(
                                    "process environment values must be strings or false",
                                ));
                            }
                        };
                        environment.push(ProcessEnvironmentChange { name, value });
                    }
                    environment.sort();
                }
                "stdin" => {
                    let Value::String(input) = value else {
                        return Err(WombatError::configuration(
                            "process `stdin` must be a string",
                        ));
                    };
                    stdin = Some(input.as_bytes().to_vec());
                }
                "timeout" => {
                    let seconds = match value {
                        Value::Integer(value) if value > 0 => value as f64,
                        Value::Number(value) if value.is_finite() && value > 0.0 => value,
                        _ => {
                            return Err(WombatError::configuration(
                                "process `timeout` must be a positive number",
                            ));
                        }
                    };
                    timeout_ms = Some((seconds * 1000.0).round() as u64);
                }
                "max_output" => {
                    let Value::Integer(value) = value else {
                        return Err(WombatError::configuration(
                            "process `max_output` must be an integer",
                        ));
                    };
                    max_output = u64::try_from(value).map_err(|_| {
                        WombatError::configuration("process `max_output` must be positive")
                    })?;
                    if max_output == 0 || max_output > MAX_PROCESS_OUTPUT {
                        return Err(WombatError::configuration(
                            "process `max_output` must be between 1 and 67108864",
                        ));
                    }
                }
                "sensitive" => {
                    let Value::Boolean(value) = value else {
                        return Err(WombatError::configuration(
                            "process `sensitive` must be boolean",
                        ));
                    };
                    sensitive = value;
                }
                "shell" => {
                    let Value::String(value) = value else {
                        return Err(WombatError::configuration(
                            "process `shell` must be a string",
                        ));
                    };
                    let value = value.to_str()?.to_string();
                    if !Path::new(&value).is_absolute() {
                        return Err(WombatError::configuration(
                            "process `shell` must be an absolute path",
                        ));
                    }
                    shell = Some(value);
                }
                _ => {
                    return Err(WombatError::configuration(format!(
                        "unknown process option `{key}`"
                    )));
                }
            }
        }
    }
    if !cwd.is_dir() {
        return Err(WombatError::configuration(format!(
            "process cwd `{}` is not a directory",
            cwd.display()
        )));
    }
    Ok(ProcessOptions {
        cwd,
        cwd_display,
        environment,
        stdin,
        timeout_ms,
        max_output,
        sensitive,
        shell,
    })
}

pub(super) fn lua_string_array(value: Value, context: &str) -> Result<Vec<String>> {
    let Value::Table(values) = value else {
        return Err(WombatError::configuration(format!(
            "{context} must be an array of strings"
        )));
    };
    let mut output = Vec::new();
    for value in values.sequence_values::<String>() {
        let value = value?;
        reject_nul(&value, context)?;
        output.push(value);
    }
    if values.raw_len() != output.len() {
        return Err(WombatError::configuration(format!(
            "{context} must be a contiguous array of strings"
        )));
    }
    Ok(output)
}

pub(super) fn reject_nul(value: &str, context: &str) -> Result<()> {
    if value.contains('\0') {
        Err(WombatError::configuration(format!(
            "{context} must not contain NUL bytes"
        )))
    } else {
        Ok(())
    }
}

pub(super) fn process_result(
    lua: &Lua,
    ok: bool,
    code: Option<i32>,
    signal: Option<i32>,
    stdout: &[u8],
    stderr: &[u8],
    location: Location,
) -> Result<Value> {
    let result = lua.create_table()?;
    result.set("ok", ok)?;
    result.set("code", code)?;
    result.set("signal", signal)?;
    result.set("stdout", lua.create_string(stdout)?)?;
    result.set("stderr", lua.create_string(stderr)?)?;
    let check = lua.create_function(move |_, table: Table| {
        if table.get::<bool>("ok")? {
            return Ok(table);
        }
        let stderr: mlua::LuaString = table.get("stderr")?;
        Err::<Table, _>(mlua::Error::external(WombatError::configuration(format!(
            "construction process failed at {}: {}",
            location.display(),
            String::from_utf8_lossy(&stderr.as_bytes())
        ))))
    })?;
    result.set("check", check)?;
    let meta = lua.create_table()?;
    meta.set(
        "__newindex",
        lua.create_function(|_, (): ()| {
            Err::<(), _>(mlua::Error::external("process results are immutable"))
        })?,
    )?;
    result.set_metatable(Some(meta))?;
    Ok(Value::Table(result))
}
