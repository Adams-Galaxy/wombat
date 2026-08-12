//! Tracked Lua source loading and module preloading.

use super::*;

pub(super) fn configure_package_path(
    lua: &Lua,
    root: &Path,
    state: Rc<RefCell<RuntimeState>>,
) -> Result<()> {
    let package: Table = lua.globals().get("package")?;
    let library = root.join("lua").to_string_lossy().replace('\\', "/");
    package.set("path", format!("{library}/?.lua;{library}/?/init.lua"))?;
    let existing: Table = package.get("searchers")?;
    let searchers = lua.create_table()?;
    searchers.set(1, existing.get::<Value>(1)?)?;
    let helper_root = root.join("lua");
    searchers.set(
        2,
        lua.create_function(move |lua, name: String| {
            let relative = helper_module_path(&name).map_err(mlua::Error::external)?;
            let candidates = [
                helper_root.join(format!("{relative}.lua")),
                helper_root.join(&relative).join("init.lua"),
            ];
            let Some(path) = candidates.iter().find(|path| path.is_file()) else {
                return Ok(MultiValue::from_vec(vec![Value::String(
                    lua.create_string(format!("\n\tno repository Lua module '{}'", name))?,
                )]));
            };
            let source = load_tracked_source(&state, path).map_err(mlua::Error::external)?;
            let loader = lua
                .load(&source)
                .set_name(format!("@{}", path.to_string_lossy()))
                .into_function()?;
            Ok(MultiValue::from_vec(vec![
                Value::Function(loader),
                Value::String(lua.create_string(display_path(&state.borrow().root, path))?),
            ]))
        })?,
    )?;
    package.set("searchers", searchers)?;
    Ok(())
}

pub(super) fn helper_module_path(name: &str) -> Result<String> {
    if name.is_empty()
        || name.split('.').any(|segment| {
            segment.is_empty()
                || !segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        })
    {
        return Err(WombatError::configuration(format!(
            "invalid repository Lua module name `{name}`"
        )));
    }
    Ok(name.replace('.', "/"))
}

pub(super) fn register_preloaded_modules(
    lua: &Lua,
    state: Rc<RefCell<RuntimeState>>,
) -> Result<()> {
    let package: Table = lua.globals().get("package")?;
    let preload: Table = package.get("preload")?;
    let native = create_native_module(lua, state)?;

    preload.set(
        "_wombat",
        lua.create_function(move |_, ()| Ok(native.clone()))?,
    )?;
    preload.set(
        "wombat",
        lua.create_function(|lua, ()| {
            lua.load(WOMBAT_LUA)
                .set_name("<wombat>/init.lua")
                .eval::<Table>()
        })?,
    )?;
    Ok(())
}
