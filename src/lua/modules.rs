//! User module selection, input resolution, and immutable context proxies.

use super::*;

const NATIVE_PROXY_MARKER: &str = "__wombat_native_proxy";
const NATIVE_PROXY_KIND: &str = "__wombat_native_proxy_kind";
const NATIVE_PROXY_PATH: &str = "__wombat_native_proxy_path";
const FROZEN_PROXY_VALUE: &str = "__wombat_frozen_proxy_value";
static NATIVE_PROXY_SENTINEL: u8 = 0;

fn mark_native_proxy(metatable: &Table, kind: &str, path: Option<&str>) -> mlua::Result<()> {
    let marker = std::ptr::from_ref(&NATIVE_PROXY_SENTINEL)
        .cast_mut()
        .cast::<std::ffi::c_void>();
    metatable.raw_set(NATIVE_PROXY_MARKER, mlua::LightUserData(marker))?;
    metatable.raw_set(NATIVE_PROXY_KIND, kind)?;
    if let Some(path) = path {
        metatable.raw_set(NATIVE_PROXY_PATH, path)?;
    }
    Ok(())
}

pub(super) fn decode_source_selector(value: Value) -> mlua::Result<(String, bool)> {
    match value {
        Value::String(value) => Ok((value.to_str()?.to_string(), false)),
        Value::Table(value) => {
            let source = value
                .get::<Option<String>>("__wombat_hidden")?
                .ok_or_else(|| {
                    mlua::Error::external("artifact source must be a string or w.hidden() value")
                })?;
            Ok((source, true))
        }
        _ => Err(mlua::Error::external(
            "artifact source must be a string or w.hidden() value",
        )),
    }
}

pub(super) fn declare_module_from(
    state: &Rc<RefCell<RuntimeState>>,
    source: Value,
    explicit_target: Option<&str>,
    location: Location,
) -> Result<()> {
    let (declared, hidden) = decode_source_selector(source).map_err(WombatError::from)?;
    let selector = compile_selector(&declared, hidden)?;
    if selector.glob {
        return Err(WombatError::configuration(
            "w.module.from() requires an exact source directory, not a glob",
        ));
    }
    let mut state = state.borrow_mut();
    let name = state
        .active_module()
        .ok_or_else(|| {
            WombatError::configuration("w.module.from() may only be called from a selected module")
        })?
        .to_string();
    let record = state.modules.get(&name).expect("active module exists");
    if record.declarations_started {
        return Err(WombatError::configuration(format!(
            "w.module.from() must run before artifact or task declarations at {}",
            location.display()
        )));
    }
    if record.source_base.is_some() {
        return Err(WombatError::configuration(format!(
            "module `{name}` declares w.module.from() more than once"
        )));
    }
    let physical_relative = if selector.physical == "." {
        String::new()
    } else {
        selector.physical.clone()
    };
    let physical = if physical_relative.is_empty() {
        "src".to_string()
    } else {
        format!("src/{physical_relative}")
    };
    let absolute = state.root.join(&physical);
    let metadata =
        fs::symlink_metadata(&absolute).map_err(|error| WombatError::io(&absolute, error))?;
    if !metadata.file_type().is_dir() {
        return Err(WombatError::configuration(format!(
            "module source base `{physical}` must be a directory"
        )));
    }
    let projection = if physical_relative.is_empty() {
        crate::model::manifest::SourceProjection {
            physical: String::new(),
            logical: String::new(),
            allocated: true,
            hidden,
            components: Vec::new(),
        }
    } else {
        project_physical(&physical_relative, hidden)?
    };
    let target = match explicit_target {
        Some(target) => Some(parse_explicit_target_root(target)?.path),
        None if projection.allocated => Some(projection.logical.clone()),
        None => None,
    };
    state
        .modules
        .get_mut(&name)
        .expect("active module exists")
        .source_base = Some(ModuleSourceBase {
        declared,
        expanded: selector.expanded,
        physical,
        logical: projection.logical,
        target,
        hidden,
    });
    Ok(())
}

pub(super) fn register_input_spec(
    state: &Rc<RefCell<RuntimeState>>,
    kind: &str,
    options: Value,
    location: Location,
) -> Result<u64> {
    let mut state = state.borrow_mut();
    if state.active_module().is_some() {
        return Err(WombatError::configuration(format!(
            "Wombat modules cannot declare project build inputs at {}",
            location.display()
        )));
    }
    if state.inputs_declared || state.root_policy_started {
        return Err(WombatError::configuration(format!(
            "input constructors must run before w.inputs(), use(), using(), or install() at {}",
            location.display()
        )));
    }
    let order = state.next_input_spec;
    state.next_input_spec = state
        .next_input_spec
        .checked_add(1)
        .ok_or_else(|| WombatError::configuration("too many project input declarations"))?;
    let spec = InputSpec::parse(order, kind, FrozenValue::from_lua(options)?, location.trace)?;
    state.input_specs.insert(order, spec);
    Ok(order)
}

pub(super) fn resolve_inputs(
    lua: &Lua,
    state: &Rc<RefCell<RuntimeState>>,
    schema: Table,
    location: Location,
) -> Result<Table> {
    let pairs = schema
        .pairs::<Value, Value>()
        .collect::<mlua::Result<Vec<_>>>()?;
    let mut entries = Vec::with_capacity(pairs.len());
    for (name, descriptor) in pairs {
        let Value::String(name) = name else {
            return Err(WombatError::configuration(
                "w.inputs() schema keys must be strings",
            ));
        };
        let Value::Integer(descriptor) = descriptor else {
            return Err(WombatError::configuration(
                "w.inputs() values must be created by w.input constructors",
            ));
        };
        entries.push((
            name.to_str()?.to_string(),
            u64::try_from(descriptor)
                .map_err(|_| WombatError::configuration("invalid Wombat input descriptor"))?,
        ));
    }

    let resolved = {
        let mut state = state.borrow_mut();
        if state.active_module().is_some() {
            return Err(WombatError::configuration(format!(
                "w.inputs() belongs to root policy and cannot run in a module at {}",
                location.display()
            )));
        }
        if state.inputs_declared {
            return Err(WombatError::configuration(format!(
                "w.inputs() may be declared only once; repeated at {}",
                location.display()
            )));
        }
        if state.root_policy_started {
            return Err(WombatError::configuration(format!(
                "w.inputs() must run before use(), using(), install(), or target-dependent policy at {}",
                location.display()
            )));
        }
        let resolved = inputs::resolve(
            entries,
            &state.input_specs,
            &state.project_arguments,
            &state.host,
        )?;
        state.inputs_declared = true;
        state.inputs = resolved.manifest.clone();
        state.project_help = resolved.help.clone();
        resolved
    };
    if resolved.help.is_some() {
        return Err(WombatError::ProjectHelpRequested);
    }
    create_values_proxy(lua, resolved.values)
}

pub(super) fn create_values_proxy(
    lua: &Lua,
    values: BTreeMap<String, FrozenValue>,
) -> Result<Table> {
    let proxy = lua.create_table()?;
    let metatable = lua.create_table()?;
    metatable.set(
        "__index",
        lua.create_function(move |lua, (_table, key): (Table, String)| {
            values
                .get(&key)
                .map_or(Ok(Value::Nil), |value| value.to_lua(lua))
        })?,
    )?;
    metatable.set(
        "__newindex",
        lua.create_function(|_, (_table, key, _value): (Table, Value, Value)| {
            Err::<(), _>(mlua::Error::external(WombatError::configuration(format!(
                "resolved build inputs are immutable; cannot assign `{key:?}`"
            ))))
        })?,
    )?;
    metatable.set("__metatable", false)?;
    proxy.set_metatable(Some(metatable))?;
    Ok(proxy)
}

pub(super) fn create_context_proxy(
    lua: &Lua,
    state: Rc<RefCell<RuntimeState>>,
    subject: ObservationSubject,
    path: String,
    callable_target: bool,
) -> mlua::Result<Table> {
    let proxy = lua.create_table()?;
    let metatable = lua.create_table()?;
    mark_native_proxy(
        &metatable,
        match subject {
            ObservationSubject::Host => "host",
            ObservationSubject::Target => "target",
        },
        Some(&path),
    )?;
    let index_state = Rc::clone(&state);
    metatable.set(
        "__index",
        lua.create_function(move |lua, (_table, key): (Table, String)| {
            let location = caller_location(lua, &index_state);
            let child_path = if path.is_empty() {
                key
            } else {
                format!("{path}.{key}")
            };
            context_access(lua, &index_state, subject, &child_path, location)
                .map_err(mlua::Error::external)
        })?,
    )?;
    metatable.set(
        "__newindex",
        lua.create_function(|_, (_table, key, _value): (Table, Value, Value)| {
            Err::<(), _>(mlua::Error::external(WombatError::configuration(format!(
                "Wombat context is immutable; cannot assign `{key:?}`"
            ))))
        })?,
    )?;
    if callable_target {
        let call_state = Rc::clone(&state);
        metatable.set(
            "__call",
            lua.create_function(move |lua, (_table, value): (Table, Value)| {
                let location = caller_location(lua, &call_state);
                set_target(&call_state, value, location)?;
                Ok(_table)
            })?,
        )?;
    }
    metatable.set("__metatable", false)?;
    proxy.set_metatable(Some(metatable))?;
    Ok(proxy)
}

pub(super) fn create_common_os_proxy(
    lua: &Lua,
    state: Rc<RefCell<RuntimeState>>,
) -> mlua::Result<Table> {
    create_common_context_proxy(lua, state, "os".to_string())
}

fn create_common_context_proxy(
    lua: &Lua,
    state: Rc<RefCell<RuntimeState>>,
    path: String,
) -> mlua::Result<Table> {
    let proxy = lua.create_table()?;
    let metatable = lua.create_table()?;
    mark_native_proxy(&metatable, "common", Some(&path))?;
    let index_state = Rc::clone(&state);
    metatable.set(
        "__index",
        lua.create_function(move |lua, (_table, key): (Table, String)| {
            let location = caller_location(lua, &index_state);
            let child_path = format!("{path}.{key}");
            common_context_access(lua, &index_state, &child_path, location)
                .map_err(mlua::Error::external)
        })?,
    )?;
    metatable.set(
        "__newindex",
        lua.create_function(|_, (_table, key, _value): (Table, Value, Value)| {
            Err::<(), _>(mlua::Error::external(WombatError::configuration(format!(
                "Wombat common context is immutable; cannot assign `{key:?}`"
            ))))
        })?,
    )?;
    metatable.set("__metatable", false)?;
    proxy.set_metatable(Some(metatable))?;
    Ok(proxy)
}

pub(super) fn common_value(
    lua: &Lua,
    state: &Rc<RefCell<RuntimeState>>,
    name: &str,
    location: Location,
) -> Result<Value> {
    let (path, predicate) = match name {
        "arch" => ("arch", None),
        "macos" => ("os.name", Some(OperatingSystemName::Macos)),
        "linux" => ("os.name", Some(OperatingSystemName::Linux)),
        "wsl" => ("wsl", None),
        _ => {
            return Err(WombatError::invariant(format!(
                "unknown common context value `{name}`"
            )));
        }
    };
    let (value, missing) = common_host_value(state, path, &location)?;
    debug_assert!(!missing, "fixed common values are always present");
    if let Some(expected) = predicate {
        return Ok(Value::Boolean(matches!(
            value,
            FrozenValue::String(ref value) if value == expected.as_str()
        )));
    }
    value.to_lua(lua).map_err(WombatError::from)
}

fn common_context_access(
    lua: &Lua,
    state: &Rc<RefCell<RuntimeState>>,
    path: &str,
    location: Location,
) -> Result<Value> {
    let (value, missing) = common_host_value(state, path, &location)?;
    if missing {
        return Ok(Value::Nil);
    }
    if matches!(value, FrozenValue::Map(_)) {
        create_common_context_proxy(lua, Rc::clone(state), path.to_string())
            .map(Value::Table)
            .map_err(WombatError::from)
    } else {
        value.to_lua(lua).map_err(WombatError::from)
    }
}

fn common_host_value(
    state: &Rc<RefCell<RuntimeState>>,
    path: &str,
    location: &Location,
) -> Result<(FrozenValue, bool)> {
    let mut state = state.borrow_mut();
    require_local_context(&mut state, location)?;
    let found = frozen_at_path(&state.host.to_frozen(), path).cloned();
    let missing = found.is_none();
    let value = found.unwrap_or(FrozenValue::Null);
    if !matches!(value, FrozenValue::Map(_)) {
        record_host_observation(&mut state, path, value.clone());
    }
    Ok((value, missing))
}

pub(super) fn create_paths_proxy(
    lua: &Lua,
    state: Rc<RefCell<RuntimeState>>,
) -> mlua::Result<Table> {
    let proxy = lua.create_table()?;
    let metatable = lua.create_table()?;
    mark_native_proxy(&metatable, "paths", None)?;
    let index_state = Rc::clone(&state);
    metatable.set(
        "__index",
        lua.create_function(move |lua, (_table, key): (Table, String)| {
            let location = caller_location(lua, &index_state);
            if key == "windows" {
                windows_paths_available(&index_state, &location)
                    .and_then(|()| {
                        create_windows_paths_proxy(lua, Rc::clone(&index_state))
                            .map_err(WombatError::from)
                    })
                    .map(Value::Table)
                    .map_err(mlua::Error::external)
            } else {
                path_value(&index_state, &key, &location)
                    .and_then(|value| value.to_lua(lua).map_err(WombatError::from))
                    .map_err(mlua::Error::external)
            }
        })?,
    )?;
    metatable.set(
        "__newindex",
        lua.create_function(|_, (_table, key, _value): (Table, Value, Value)| {
            Err::<(), _>(mlua::Error::external(WombatError::configuration(format!(
                "w.paths is immutable; cannot assign `{key:?}`"
            ))))
        })?,
    )?;
    metatable.set("__metatable", false)?;
    proxy.set_metatable(Some(metatable))?;
    Ok(proxy)
}

fn create_windows_paths_proxy(lua: &Lua, state: Rc<RefCell<RuntimeState>>) -> mlua::Result<Table> {
    let proxy = lua.create_table()?;
    let metatable = lua.create_table()?;
    mark_native_proxy(&metatable, "windows_paths", None)?;
    let index_state = Rc::clone(&state);
    metatable.set(
        "__index",
        lua.create_function(move |lua, (_table, key): (Table, String)| {
            let location = caller_location(lua, &index_state);
            windows_path_value(&index_state, &key, &location)
                .and_then(|value| value.to_lua(lua).map_err(WombatError::from))
                .map_err(mlua::Error::external)
        })?,
    )?;
    metatable.set(
        "__newindex",
        lua.create_function(|_, (_table, key, _value): (Table, Value, Value)| {
            Err::<(), _>(mlua::Error::external(WombatError::configuration(format!(
                "w.paths.windows is immutable; cannot assign `{key:?}`"
            ))))
        })?,
    )?;
    metatable.set("__metatable", false)?;
    proxy.set_metatable(Some(metatable))?;
    Ok(proxy)
}

fn path_value(
    state: &Rc<RefCell<RuntimeState>>,
    key: &str,
    location: &Location,
) -> Result<FrozenValue> {
    if key == "repository" {
        return Ok(FrozenValue::String(
            state.borrow().root.to_string_lossy().into_owned(),
        ));
    }
    let mut state = state.borrow_mut();
    require_local_context(&mut state, location)?;
    let (path, observation) = match key {
        "home" => (state.host.home.clone(), "home"),
        "local_root" => (
            state.host.home.as_ref().map(|home| home.join(".local")),
            "paths.local_root",
        ),
        "config" => (state.host.paths.config.clone(), "paths.config"),
        "data" => (state.host.paths.data.clone(), "paths.data"),
        "state" => (state.host.paths.state.clone(), "paths.state"),
        "cache" => (state.host.paths.cache.clone(), "paths.cache"),
        _ => {
            return Err(WombatError::configuration(format!(
                "unknown w.paths key `{key}`"
            )));
        }
    };
    let path = path.ok_or_else(|| {
        WombatError::configuration(format!(
            "w.paths.{key} is unavailable because HOME is not set"
        ))
    })?;
    if !path.is_absolute() {
        return Err(WombatError::configuration(format!(
            "w.paths.{key} must be absolute, got `{}`",
            path.display()
        )));
    }
    let value = FrozenValue::String(path.to_string_lossy().into_owned());
    record_host_observation(&mut state, observation, value.clone());
    Ok(value)
}

fn windows_paths_available(state: &Rc<RefCell<RuntimeState>>, location: &Location) -> Result<()> {
    let mut state = state.borrow_mut();
    require_local_context(&mut state, location)?;
    if !state.host.wsl {
        return Err(WombatError::configuration(
            "w.paths.windows is available only inside WSL",
        ));
    }
    record_host_observation(&mut state, "wsl", FrozenValue::Boolean(true));
    Ok(())
}

fn windows_path_value(
    state: &Rc<RefCell<RuntimeState>>,
    key: &str,
    location: &Location,
) -> Result<FrozenValue> {
    if key != "home" {
        return Err(WombatError::configuration(format!(
            "unknown w.paths.windows key `{key}`"
        )));
    }
    windows_paths_available(state, location)?;
    let path = match state.borrow().host.paths.windows_home.clone() {
        Some(path) => path,
        None => {
            let path = observe_windows_home()?;
            state.borrow_mut().host.paths.windows_home = Some(path.clone());
            path
        }
    };
    let mut state = state.borrow_mut();
    if !path.is_absolute() {
        return Err(WombatError::configuration(format!(
            "w.paths.windows.home must resolve to an absolute WSL path, got `{}`",
            path.display()
        )));
    }
    let value = FrozenValue::String(path.to_string_lossy().into_owned());
    record_host_observation(&mut state, "paths.windows.home", value.clone());
    Ok(value)
}

fn observe_windows_home() -> Result<std::path::PathBuf> {
    use std::process::Command;
    use std::time::Duration;

    let mut profile_command = Command::new("cmd.exe");
    profile_command.args(["/d", "/s", "/c", "echo %USERPROFILE%"]);
    let profile = crate::execution::process::run(
        &mut profile_command,
        "Windows profile lookup",
        Some(Duration::from_secs(5)),
        16 * 1024,
        None,
        crate::execution::process::Forwarding::Retained,
    )
    .map_err(|error| {
        WombatError::configuration(format!(
            "cannot resolve Windows home: cmd.exe is unavailable through WSL interop: {error}"
        ))
    })?;
    if !profile.success || profile.stdout.truncated {
        return Err(WombatError::configuration(format!(
            "cannot resolve Windows home: cmd.exe did not return a complete profile path ({})",
            profile.status
        )));
    }
    let profile = String::from_utf8(profile.stdout.bytes).map_err(|_| {
        WombatError::configuration("cannot resolve Windows home: cmd.exe returned non-UTF-8 output")
    })?;
    let profile = profile.trim();
    if profile.is_empty() {
        return Err(WombatError::configuration(
            "cannot resolve Windows home: USERPROFILE is empty in Windows interop",
        ));
    }
    let mut translation_command = Command::new("wslpath");
    translation_command.args(["-u", profile]);
    let translated = crate::execution::process::run(
        &mut translation_command,
        "Windows profile translation",
        Some(Duration::from_secs(5)),
        16 * 1024,
        None,
        crate::execution::process::Forwarding::Retained,
    )
    .map_err(|error| {
        WombatError::configuration(format!(
            "cannot translate Windows home: wslpath is unavailable: {error}"
        ))
    })?;
    if !translated.success || translated.stdout.truncated {
        return Err(WombatError::configuration(format!(
            "cannot translate Windows home with wslpath: {}",
            translated.status
        )));
    }
    let path = String::from_utf8(translated.stdout.bytes).map_err(|_| {
        WombatError::configuration(
            "cannot translate Windows home: wslpath returned non-UTF-8 output",
        )
    })?;
    let path = path.trim();
    crate::model::path::validate_absolute_target(path)?;
    Ok(std::path::PathBuf::from(path))
}

fn require_local_context(state: &mut RuntimeState, location: &Location) -> Result<()> {
    if state.target_first_read.is_none() {
        state.target_first_read = Some(location.clone());
    }
    if !state
        .target
        .platform
        .locally_compatible_with(&state.host.platform)
    {
        return Err(WombatError::configuration(format!(
            "common local context is unavailable because build target `{}` differs from host `{}` at {}; use w.host or w.target explicitly",
            state.target.platform.compact(),
            state.host.platform.compact(),
            location.display()
        )));
    }
    Ok(())
}

fn record_host_observation(state: &mut RuntimeState, path: &str, value: FrozenValue) {
    state
        .observations
        .entry((ObservationSubject::Host, path.to_string()))
        .or_insert_with(|| Observation {
            subject: ObservationSubject::Host,
            path: path.to_string(),
            value,
        });
}

pub(super) fn resolve_template_context(
    lua: &Lua,
    state: &Rc<RefCell<RuntimeState>>,
    value: Value,
    location: Location,
) -> Result<Value> {
    let resolved = FrozenValue::from_lua_resolving(value, |table| {
        resolve_lazy_proxy(table, state, &location)
    })?;
    if !matches!(resolved, FrozenValue::Map(_)) {
        return Err(WombatError::configuration(
            "w.template.context() requires a string-keyed map",
        ));
    }
    resolved.to_lua(lua).map_err(WombatError::from)
}

fn resolve_lazy_proxy(
    table: &Table,
    state: &Rc<RefCell<RuntimeState>>,
    location: &Location,
) -> Result<Option<FrozenValue>> {
    let Some(metatable) = table.metatable() else {
        return Ok(None);
    };
    let marker = metatable
        .raw_get::<Value>(NATIVE_PROXY_MARKER)
        .map_err(WombatError::from)?;
    let expected = std::ptr::from_ref(&NATIVE_PROXY_SENTINEL)
        .cast_mut()
        .cast::<std::ffi::c_void>();
    if !matches!(marker, Value::LightUserData(value) if value.0 == expected) {
        return Ok(None);
    }
    let kind = metatable
        .raw_get::<String>(NATIVE_PROXY_KIND)
        .map_err(WombatError::from)?;
    if kind == "frozen" {
        let value = metatable
            .raw_get::<Value>(FROZEN_PROXY_VALUE)
            .map_err(WombatError::from)?;
        return FrozenValue::from_lua(value).map(Some);
    }
    let path = metatable
        .raw_get::<Option<String>>(NATIVE_PROXY_PATH)
        .map_err(WombatError::from)?
        .unwrap_or_default();
    match kind.as_str() {
        "host" => snapshot_explicit_context(state, ObservationSubject::Host, &path, location),
        "target" => snapshot_explicit_context(state, ObservationSubject::Target, &path, location),
        "common" => snapshot_common_context(state, &path, location),
        "paths" => snapshot_paths(state, location),
        "windows_paths" => snapshot_windows_paths(state, location),
        _ => Err(WombatError::invariant(format!(
            "unknown native lazy proxy kind `{kind}`"
        ))),
    }
    .map(Some)
}

fn snapshot_explicit_context(
    state: &Rc<RefCell<RuntimeState>>,
    subject: ObservationSubject,
    path: &str,
    location: &Location,
) -> Result<FrozenValue> {
    let mut state = state.borrow_mut();
    if subject == ObservationSubject::Target && state.target_first_read.is_none() {
        state.target_first_read = Some(location.clone());
    }
    let root = match subject {
        ObservationSubject::Host => state.host.to_frozen(),
        ObservationSubject::Target => effective_target(&state).to_frozen(),
    };
    let value = snapshot_at_path(&root, path)?;
    record_context_snapshot(&mut state, subject, path, &value);
    Ok(value)
}

fn snapshot_common_context(
    state: &Rc<RefCell<RuntimeState>>,
    path: &str,
    location: &Location,
) -> Result<FrozenValue> {
    let mut state = state.borrow_mut();
    require_local_context(&mut state, location)?;
    let value = snapshot_at_path(&state.host.to_frozen(), path)?;
    record_context_snapshot(&mut state, ObservationSubject::Host, path, &value);
    Ok(value)
}

fn snapshot_paths(state: &Rc<RefCell<RuntimeState>>, location: &Location) -> Result<FrozenValue> {
    let mut paths = BTreeMap::new();
    for key in [
        "repository",
        "home",
        "local_root",
        "config",
        "data",
        "state",
        "cache",
    ] {
        paths.insert(key.to_string(), path_value(state, key, location)?);
    }
    if state.borrow().host.wsl {
        paths.insert(
            "windows".to_string(),
            snapshot_windows_paths(state, location)?,
        );
    }
    Ok(FrozenValue::Map(paths))
}

fn snapshot_windows_paths(
    state: &Rc<RefCell<RuntimeState>>,
    location: &Location,
) -> Result<FrozenValue> {
    Ok(FrozenValue::Map(BTreeMap::from([(
        "home".to_string(),
        windows_path_value(state, "home", location)?,
    )])))
}

fn snapshot_at_path(root: &FrozenValue, path: &str) -> Result<FrozenValue> {
    if path.is_empty() {
        return Ok(root.clone());
    }
    frozen_at_path(root, path).cloned().ok_or_else(|| {
        WombatError::invariant(format!(
            "native lazy context path `{path}` no longer exists"
        ))
    })
}

fn record_context_snapshot(
    state: &mut RuntimeState,
    subject: ObservationSubject,
    path: &str,
    value: &FrozenValue,
) {
    if let FrozenValue::Map(values) = value {
        for (key, value) in values {
            let child = if path.is_empty() {
                key.clone()
            } else {
                format!("{path}.{key}")
            };
            record_context_snapshot(state, subject, &child, value);
        }
    } else if !path.is_empty() && !is_foundational_target(subject, path) {
        state
            .observations
            .entry((subject, path.to_string()))
            .or_insert_with(|| Observation {
                subject,
                path: path.to_string(),
                value: value.clone(),
            });
    }
}

pub(super) fn context_access(
    lua: &Lua,
    state: &Rc<RefCell<RuntimeState>>,
    subject: ObservationSubject,
    path: &str,
    location: Location,
) -> Result<Value> {
    let (value, missing) = {
        let mut state = state.borrow_mut();
        if subject == ObservationSubject::Target && state.target_first_read.is_none() {
            state.target_first_read = Some(location);
        }
        let root = match subject {
            ObservationSubject::Host => state.host.to_frozen(),
            ObservationSubject::Target => effective_target(&state).to_frozen(),
        };
        let found = frozen_at_path(&root, path).cloned();
        let missing = found.is_none();
        let value = found.unwrap_or(FrozenValue::Null);
        if !matches!(value, FrozenValue::Map(_)) && !is_foundational_target(subject, path) {
            state
                .observations
                .entry((subject, path.to_string()))
                .or_insert_with(|| Observation {
                    subject,
                    path: path.to_string(),
                    value: value.clone(),
                });
        }
        (value, missing)
    };
    if missing {
        return Ok(Value::Nil);
    }
    if matches!(value, FrozenValue::Map(_)) {
        create_context_proxy(lua, Rc::clone(state), subject, path.to_string(), false)
            .map(Value::Table)
            .map_err(WombatError::from)
    } else {
        readonly_frozen(lua, value).map_err(WombatError::from)
    }
}

pub(super) fn readonly_frozen(lua: &Lua, value: FrozenValue) -> mlua::Result<Value> {
    match value {
        FrozenValue::Map(values) => {
            let proxy = lua.create_table()?;
            let metatable = lua.create_table()?;
            mark_native_proxy(&metatable, "frozen", None)?;
            metatable.raw_set(
                FROZEN_PROXY_VALUE,
                FrozenValue::Map(values.clone()).to_lua(lua)?,
            )?;
            metatable.set(
                "__index",
                lua.create_function(move |lua, (_table, key): (Table, String)| {
                    values
                        .get(&key)
                        .cloned()
                        .map_or(Ok(Value::Nil), |value| readonly_frozen(lua, value))
                })?,
            )?;
            metatable.set(
                "__newindex",
                lua.create_function(|_, (_table, _key, _value): (Table, Value, Value)| {
                    Err::<(), _>(mlua::Error::external(WombatError::configuration(
                        "resolved Wombat values are immutable",
                    )))
                })?,
            )?;
            metatable.set("__metatable", false)?;
            proxy.set_metatable(Some(metatable))?;
            Ok(Value::Table(proxy))
        }
        FrozenValue::Array(values) => {
            let proxy = lua.create_table()?;
            let metatable = lua.create_table()?;
            mark_native_proxy(&metatable, "frozen", None)?;
            metatable.raw_set(
                FROZEN_PROXY_VALUE,
                FrozenValue::Array(values.clone()).to_lua(lua)?,
            )?;
            let length = values.len();
            metatable.set(
                "__index",
                lua.create_function(move |lua, (_table, index): (Table, i64)| {
                    usize::try_from(index)
                        .ok()
                        .and_then(|index| index.checked_sub(1))
                        .and_then(|index| values.get(index).cloned())
                        .map_or(Ok(Value::Nil), |value| readonly_frozen(lua, value))
                })?,
            )?;
            metatable.set("__len", lua.create_function(move |_, _: Table| Ok(length))?)?;
            metatable.set(
                "__newindex",
                lua.create_function(|_, (_table, _key, _value): (Table, Value, Value)| {
                    Err::<(), _>(mlua::Error::external(WombatError::configuration(
                        "Wombat context arrays are immutable",
                    )))
                })?,
            )?;
            metatable.set("__metatable", false)?;
            proxy.set_metatable(Some(metatable))?;
            Ok(Value::Table(proxy))
        }
        other => other.to_lua(lua),
    }
}

pub(super) fn frozen_at_path<'a>(root: &'a FrozenValue, path: &str) -> Option<&'a FrozenValue> {
    path.split('.')
        .try_fold(root, |value, component| match value {
            FrozenValue::Map(map) => map.get(component),
            _ => None,
        })
}

pub(super) fn is_foundational_target(subject: ObservationSubject, path: &str) -> bool {
    subject == ObservationSubject::Target && matches!(path, "os.name" | "arch")
}
