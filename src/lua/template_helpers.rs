//! Source-backed Lua functions exposed as deterministic Handlebars helpers.
//!
//! Registration records names during construction, but the functions are
//! validated in a separate constrained Lua state and executed only from the
//! frozen plan payload. This keeps render behavior out of the configuration VM
//! and prevents a live repository edit from changing an already constructed
//! plan.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::Path;
use std::rc::Rc;

use mlua::{Function, Lua, LuaOptions, MultiValue, StdLib, Table, Value};

use super::{
    Location, RuntimeState, caller_location, display_path, helper_module_path, load_tracked_source,
};
use crate::model::manifest::{
    SourceFile, SourceLocation, SourceTrace, TemplateHelperExport, TemplateHelperPack,
};
use crate::{Diagnostic, Result, WombatError};

pub(crate) const CONTRACT_VERSION: u32 = 1;
pub(crate) const INSTRUCTION_LIMIT: u64 = 10_000_000;
pub(crate) const MEMORY_LIMIT: usize = 16 * 1024 * 1024;
const PAYLOAD_ROOT: &str = "payloads/helpers";

const RESERVED_HELPERS: &[&str] = &[
    "and",
    "blockHelperMissing",
    "coalesce",
    "each",
    "eq",
    "gt",
    "gte",
    "helperMissing",
    "if",
    "len",
    "log",
    "lookup",
    "lt",
    "lte",
    "ne",
    "not",
    "or",
    "raw",
    "unless",
    "with",
];

#[derive(Clone, Debug)]
pub(super) struct Declaration {
    module: String,
    prefix: String,
    location: Location,
}

#[derive(Default)]
struct LoaderState {
    cache: BTreeMap<String, mlua::RegistryKey>,
    loading: Vec<String>,
    edges: BTreeMap<String, BTreeSet<String>>,
    sources: BTreeMap<String, SourceFile>,
    module_sources: BTreeMap<String, String>,
}

pub(super) fn declare(
    state: &Rc<RefCell<RuntimeState>>,
    module: String,
    prefix: String,
    location: Location,
) -> Result<()> {
    helper_module_path(&module)?;
    if !prefix
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(diagnostic_at(
            format!("template helper prefix `{prefix}` contains unsupported characters"),
            &location.trace,
        ));
    }
    state
        .borrow_mut()
        .template_helper_declarations
        .push(Declaration {
            module,
            prefix,
            location,
        });
    Ok(())
}

pub(super) fn finalize(state: &Rc<RefCell<RuntimeState>>) -> Result<Vec<TemplateHelperPack>> {
    let mut declarations = BTreeMap::<(String, String), Location>::new();
    for declaration in &state.borrow().template_helper_declarations {
        declarations
            .entry((declaration.module.clone(), declaration.prefix.clone()))
            .or_insert_with(|| declaration.location.clone());
    }
    if declarations.is_empty() {
        return Ok(Vec::new());
    }

    let lua = constrained_lua()?;
    let loader = Rc::new(RefCell::new(LoaderState::default()));
    install_require(&lua, Rc::clone(state), Rc::clone(&loader))?;

    let mut packs = Vec::with_capacity(declarations.len());
    let mut public_names = BTreeMap::<String, (String, SourceTrace)>::new();
    for ((module, prefix), location) in declarations {
        let value = load_module(&lua, state, &loader, &module)
            .map_err(|error| helper_load_error(state, &module, &location.trace, error))?;
        let Value::Table(exports) = value else {
            return Err(diagnostic_at(
                format!("template helper pack `{module}` must return a table of functions"),
                &location.trace,
            ));
        };
        if exports.metatable().is_some() {
            return Err(diagnostic_at(
                format!("template helper pack `{module}` must return a plain table"),
                &location.trace,
            ));
        }
        let mut resolved = Vec::new();
        for pair in exports.pairs::<Value, Value>() {
            let (key, value) = pair.map_err(WombatError::from)?;
            let Value::String(key) = key else {
                return Err(diagnostic_at(
                    format!("template helper pack `{module}` exports must use string keys"),
                    &location.trace,
                ));
            };
            let export = key.to_str().map_err(WombatError::from)?.to_owned();
            let Value::Function(function) = value else {
                return Err(diagnostic_at(
                    format!("template helper pack `{module}` export `{export}` must be a function"),
                    &location.trace,
                ));
            };
            let name = format!("{prefix}{export}");
            validate_public_name(&name).map_err(|error| {
                error.with_note(format!(
                    "exported by template helper pack `{module}` declared at {}",
                    location.trace
                ))
            })?;
            if RESERVED_HELPERS.binary_search(&name.as_str()).is_ok() {
                return Err(diagnostic_at(
                    format!("template helper `{name}` cannot replace a built-in helper"),
                    &location.trace,
                ));
            }
            if let Some((owner, previous)) = public_names.get(&name) {
                let mut error = diagnostic_at(
                    format!(
                        "template helper `{name}` is exported by both `{owner}` and `{module}`"
                    ),
                    &location.trace,
                );
                if let WombatError::Diagnostic(diagnostic) = &mut error {
                    diagnostic
                        .notes
                        .push(format!("first declared at {previous}"));
                }
                return Err(error);
            }
            let info = function.info();
            if info.what != "Lua" {
                return Err(diagnostic_at(
                    format!("template helper `{name}` must be implemented in Lua"),
                    &location.trace,
                ));
            }
            let defined_at = function_location(state, &module, &function);
            public_names.insert(name.clone(), (module.clone(), location.trace.clone()));
            resolved.push(TemplateHelperExport {
                export,
                name,
                defined_at,
            });
        }
        if resolved.is_empty() {
            return Err(diagnostic_at(
                format!("template helper pack `{module}` must export at least one function"),
                &location.trace,
            ));
        }
        resolved.sort_by(|left, right| left.name.cmp(&right.name));
        let sources = transitive_sources(&loader.borrow(), &module)?;
        packs.push(TemplateHelperPack {
            contract_version: CONTRACT_VERSION,
            module,
            prefix,
            exports: resolved,
            sources,
            declared_at: location.trace,
        });
    }
    validate_catalog(&packs)?;
    Ok(packs)
}

fn constrained_lua() -> Result<Lua> {
    let lua = Lua::new_with(
        StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8,
        LuaOptions::default(),
    )?;
    lua.set_memory_limit(MEMORY_LIMIT)?;
    for global in [
        "collectgarbage",
        "dofile",
        "load",
        "loadfile",
        "print",
        "warn",
    ] {
        lua.globals().set(global, Value::Nil)?;
    }
    let math: Table = lua.globals().get("math")?;
    math.set("random", Value::Nil)?;
    math.set("randomseed", Value::Nil)?;
    let instructions = Rc::new(Cell::new(0_u64));
    lua.set_hook(
        mlua::HookTriggers::new().every_nth_instruction(10_000),
        move |_, _| {
            let next = instructions.get().saturating_add(10_000);
            instructions.set(next);
            if next > INSTRUCTION_LIMIT {
                Err(mlua::Error::runtime(format!(
                    "template helper exceeded the {INSTRUCTION_LIMIT} instruction limit"
                )))
            } else {
                Ok(mlua::VmState::Continue)
            }
        },
    )?;
    Ok(lua)
}

fn install_require(
    lua: &Lua,
    state: Rc<RefCell<RuntimeState>>,
    loader: Rc<RefCell<LoaderState>>,
) -> Result<()> {
    let api = minimal_wombat_api(lua)?;
    lua.globals().set(
        "require",
        lua.create_function(move |lua, module: String| {
            if module == "wombat" {
                return Ok(Value::Table(api.clone()));
            }
            load_module(lua, &state, &loader, &module).map_err(mlua::Error::external)
        })?,
    )?;
    Ok(())
}

fn minimal_wombat_api(lua: &Lua) -> Result<Table> {
    let api = lua.create_table()?;
    api.set("null", Value::NULL)?;
    api.set(
        "array",
        lua.create_function(|lua, value: Option<Table>| {
            let table = value.map_or_else(|| lua.create_table(), Ok)?;
            match crate::model::frozen::FrozenValue::from_lua(Value::Table(table.clone()))
                .map_err(mlua::Error::external)?
            {
                crate::model::frozen::FrozenValue::Array(_) => {}
                crate::model::frozen::FrozenValue::Map(values) if values.is_empty() => {}
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
    Ok(api)
}

fn load_module(
    lua: &Lua,
    state: &Rc<RefCell<RuntimeState>>,
    loader: &Rc<RefCell<LoaderState>>,
    module: &str,
) -> Result<Value> {
    let relative = helper_module_path(module)?;
    let parent = loader.borrow().loading.last().cloned();
    if let Some(parent) = parent {
        loader
            .borrow_mut()
            .edges
            .entry(parent)
            .or_default()
            .insert(module.to_string());
    }
    if let Some(value) = loader
        .borrow()
        .cache
        .get(module)
        .map(|key| lua.registry_value(key))
    {
        return value.map_err(WombatError::from);
    }
    if loader.borrow().loading.iter().any(|item| item == module) {
        return Err(WombatError::configuration(format!(
            "template helper module dependency cycle includes `{module}`"
        )));
    }
    let root = state.borrow().root.join("lua");
    let direct = root.join(format!("{relative}.lua"));
    let initial = root.join(&relative).join("init.lua");
    let path = if direct.is_file() {
        direct
    } else if initial.is_file() {
        initial
    } else {
        return Err(WombatError::configuration(format!(
            "cannot find template helper module `{module}` under `lua/`"
        )));
    };
    let source = load_tracked_source(state, &path)?;
    let portable = display_path(&state.borrow().root, &path);
    let manifest = state
        .borrow()
        .sources
        .get(&portable)
        .expect("tracked helper source exists")
        .manifest
        .clone();
    {
        let mut loader = loader.borrow_mut();
        loader.sources.insert(portable.clone(), manifest);
        loader
            .module_sources
            .insert(module.to_string(), portable.clone());
        loader.loading.push(module.to_string());
    }
    let result = lua
        .load(&source)
        .set_name(format!("@{portable}"))
        .eval::<MultiValue>();
    loader.borrow_mut().loading.pop();
    let mut values = result.map_err(WombatError::from)?;
    let value = match values.len() {
        0 => Value::Boolean(true),
        1 => values.pop_front().expect("one module return"),
        _ => {
            return Err(WombatError::configuration(format!(
                "template helper module `{module}` must return exactly one value"
            )));
        }
    };
    let key = lua.create_registry_value(value.clone())?;
    loader.borrow_mut().cache.insert(module.to_string(), key);
    Ok(value)
}

fn transitive_sources(loader: &LoaderState, root: &str) -> Result<Vec<SourceFile>> {
    fn visit(module: &str, loader: &LoaderState, seen: &mut BTreeSet<String>) -> Result<()> {
        if !seen.insert(module.to_string()) {
            return Ok(());
        }
        if !loader.module_sources.contains_key(module) {
            return Err(WombatError::invariant(format!(
                "loaded helper module `{module}` has no source identity"
            )));
        }
        if let Some(dependencies) = loader.edges.get(module) {
            for dependency in dependencies {
                visit(dependency, loader, seen)?;
            }
        }
        Ok(())
    }
    let mut modules = BTreeSet::new();
    visit(root, loader, &mut modules)?;
    let mut sources = modules
        .into_iter()
        .map(|module| {
            let path = &loader.module_sources[&module];
            loader.sources[path].clone()
        })
        .collect::<Vec<_>>();
    sources.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(sources)
}

fn function_location(
    state: &Rc<RefCell<RuntimeState>>,
    module: &str,
    function: &Function,
) -> SourceLocation {
    let info = function.info();
    let source = info
        .source
        .as_deref()
        .and_then(|source| source.strip_prefix('@'))
        .map(|source| display_path(&state.borrow().root, Path::new(source)))
        .unwrap_or_else(|| format!("lua/{}.lua", module.replace('.', "/")));
    SourceLocation {
        source,
        line: info.line_defined.and_then(|line| u32::try_from(line).ok()),
        column: None,
    }
}

fn validate_public_name(name: &str) -> Result<()> {
    let mut bytes = name.bytes();
    if !bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(WombatError::configuration(format!(
            "invalid template helper name `{name}`; expected an ASCII letter or `_` followed by letters, numbers, `_`, or `-`"
        )));
    }
    Ok(())
}

pub(crate) fn validate_catalog(packs: &[TemplateHelperPack]) -> Result<()> {
    let mut previous_pack: Option<(&str, &str)> = None;
    let mut names = BTreeSet::new();
    for pack in packs {
        let key = (pack.module.as_str(), pack.prefix.as_str());
        if previous_pack.is_some_and(|previous| previous >= key) {
            return Err(WombatError::corrupt_state(
                "template helper packs are not uniquely sorted",
            ));
        }
        previous_pack = Some(key);
        if pack.contract_version != CONTRACT_VERSION {
            return Err(WombatError::corrupt_state(format!(
                "unsupported template helper contract version {} for `{}`",
                pack.contract_version, pack.module
            )));
        }
        helper_module_path(&pack.module)?;
        if pack.exports.is_empty() || pack.sources.is_empty() {
            return Err(WombatError::corrupt_state(format!(
                "template helper pack `{}` has no exports or source closure",
                pack.module
            )));
        }
        let mut previous_export = None;
        for export in &pack.exports {
            validate_public_name(&export.name)?;
            if export.export.is_empty()
                || previous_export.is_some_and(|previous: &str| previous >= export.name.as_str())
                || !names.insert(export.name.as_str())
                || RESERVED_HELPERS
                    .binary_search(&export.name.as_str())
                    .is_ok()
            {
                return Err(WombatError::corrupt_state(format!(
                    "template helper pack `{}` has invalid or conflicting exports",
                    pack.module
                )));
            }
            previous_export = Some(export.name.as_str());
        }
        let mut previous_source = None;
        for source in &pack.sources {
            crate::model::path::validate_relative_path(&source.path, "template helper source")?;
            if !source.path.starts_with("lua/")
                || previous_source.is_some_and(|previous: &str| previous >= source.path.as_str())
                || !valid_digest(&source.digest)
            {
                return Err(WombatError::corrupt_state(format!(
                    "template helper pack `{}` has an invalid source closure",
                    pack.module
                )));
            }
            previous_source = Some(source.path.as_str());
        }
    }
    Ok(())
}

pub(crate) fn publish_payloads(
    source_root: &Path,
    plan_root: &Path,
    packs: &[TemplateHelperPack],
) -> Result<()> {
    let sources = source_catalog(packs)?;
    if sources.is_empty() {
        return Ok(());
    }
    let root = plan_root.join(PAYLOAD_ROOT);
    crate::storage::permissions::ensure_private_directory(&root)?;
    for source in sources.values() {
        let path = source_root.join(&source.path);
        crate::model::source::validate_source_components(source_root, &path)?;
        let bytes = fs::read(&path).map_err(|error| WombatError::io(&path, error))?;
        if crate::storage::digest::sha256(&bytes) != source.digest {
            return Err(WombatError::configuration(format!(
                "template helper source `{}` changed during plan publication",
                source.path
            )));
        }
        let destination = root.join(&source.path);
        let parent = crate::storage::path::parent(&destination)?;
        crate::storage::permissions::ensure_private_directory(parent)?;
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&destination)
            .map_err(|error| WombatError::io(&destination, error))?;
        output
            .write_all(&bytes)
            .map_err(|error| WombatError::io(&destination, error))?;
        crate::storage::permissions::set_private_file(&output, &destination)?;
    }
    Ok(())
}

pub(crate) fn verify_payloads(plan_root: &Path, packs: &[TemplateHelperPack]) -> Result<()> {
    let expected = source_catalog(packs)?;
    let root = plan_root.join(PAYLOAD_ROOT);
    if expected.is_empty() {
        if root
            .try_exists()
            .map_err(|error| WombatError::io(&root, error))?
        {
            return Err(WombatError::corrupt_state(
                "plan contains an unexpected template helper payload tree",
            ));
        }
        return Ok(());
    }
    let metadata = fs::symlink_metadata(&root).map_err(|error| WombatError::io(&root, error))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(WombatError::corrupt_state(
            "template helper payload root must be a plain directory",
        ));
    }
    let mut expected_directories = BTreeSet::new();
    for path in expected.keys() {
        let mut current = Path::new(path).parent();
        while let Some(parent) = current {
            if parent.as_os_str().is_empty() {
                break;
            }
            expected_directories.insert(parent.to_string_lossy().replace('\\', "/"));
            current = parent.parent();
        }
    }
    let mut found = BTreeSet::new();
    let mut pending = vec![root.clone()];
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
                return Err(WombatError::corrupt_state(format!(
                    "template helper payload `{}` must not be a symbolic link",
                    path.display()
                )));
            }
            if metadata.is_dir() {
                let relative = path
                    .strip_prefix(&root)
                    .expect("helper payload walk remains beneath root")
                    .to_string_lossy()
                    .replace('\\', "/");
                if !expected_directories.contains(&relative) {
                    return Err(WombatError::corrupt_state(format!(
                        "unexpected template helper payload directory `{relative}`"
                    )));
                }
                pending.push(path);
                continue;
            }
            if !metadata.is_file() {
                return Err(WombatError::corrupt_state(format!(
                    "template helper payload `{}` must be a regular file",
                    path.display()
                )));
            }
            let relative = path
                .strip_prefix(&root)
                .expect("helper payload walk remains beneath root")
                .to_string_lossy()
                .replace('\\', "/");
            let Some(source) = expected.get(&relative) else {
                return Err(WombatError::corrupt_state(format!(
                    "unexpected template helper payload `{relative}`"
                )));
            };
            let bytes = fs::read(&path).map_err(|error| WombatError::io(&path, error))?;
            if crate::storage::digest::sha256(&bytes) != source.digest {
                return Err(WombatError::corrupt_state(format!(
                    "template helper payload `{relative}` failed verification"
                )));
            }
            found.insert(relative);
        }
    }
    let missing = expected
        .keys()
        .filter(|path| !found.contains(path.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(WombatError::corrupt_state(format!(
            "template helper payload tree is missing {}",
            missing.join(", ")
        )));
    }
    Ok(())
}

fn source_catalog(packs: &[TemplateHelperPack]) -> Result<BTreeMap<String, SourceFile>> {
    let mut sources = BTreeMap::<String, SourceFile>::new();
    for source in packs.iter().flat_map(|pack| &pack.sources) {
        if let Some(existing) = sources.insert(source.path.clone(), source.clone())
            && existing != *source
        {
            return Err(WombatError::corrupt_state(format!(
                "template helper source `{}` has conflicting identities",
                source.path
            )));
        }
    }
    Ok(sources)
}

fn valid_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn helper_load_error(
    state: &Rc<RefCell<RuntimeState>>,
    module: &str,
    declaration: &SourceTrace,
    error: WombatError,
) -> WombatError {
    let raw = error.to_string();
    let (source, line) = helper_error_location(state, module, &raw);
    let mut diagnostic = Diagnostic::new(format!(
        "failed to load template helper pack `{module}`: {}",
        super::clean_lua_reason(&raw)
    ));
    diagnostic.primary = Some(SourceLocation {
        source: source.clone(),
        line,
        column: None,
    });
    diagnostic.source_line = line.and_then(|line| {
        state
            .borrow()
            .sources
            .get(&source)?
            .snapshot
            .lines()
            .nth(line.saturating_sub(1) as usize)
            .map(str::to_owned)
    });
    diagnostic
        .notes
        .push(format!("registered at {declaration}"));
    diagnostic.underlying = Some(raw);
    WombatError::diagnostic(diagnostic)
}

fn helper_error_location(
    state: &Rc<RefCell<RuntimeState>>,
    module: &str,
    raw: &str,
) -> (String, Option<u32>) {
    let state = state.borrow();
    if let Some((_, source, line)) = state
        .sources
        .keys()
        .filter_map(|source| {
            let offset = raw.find(source)?;
            let suffix = raw[offset + source.len()..].strip_prefix(':')?;
            let digits = suffix.bytes().take_while(u8::is_ascii_digit).count();
            let line = suffix[..digits].parse().ok()?;
            Some((offset, source.clone(), line))
        })
        .min_by_key(|(offset, _, _)| *offset)
    {
        return (source, Some(line));
    }
    let source = state
        .sources
        .keys()
        .find(|path| {
            path.ends_with(&format!("/{}.lua", module.replace('.', "/")))
                || path.ends_with(&format!("/{}/init.lua", module.replace('.', "/")))
        })
        .cloned()
        .unwrap_or_else(|| format!("lua/{}.lua", module.replace('.', "/")));
    (source, super::parse_lua_line(raw))
}

fn diagnostic_at(message: String, trace: &SourceTrace) -> WombatError {
    let mut diagnostic = Diagnostic::new(message);
    diagnostic.primary = Some(trace.primary.clone());
    diagnostic.user_frames = std::iter::once(trace.primary.clone())
        .chain(trace.callers.iter().cloned())
        .collect();
    WombatError::diagnostic(diagnostic)
}

#[derive(Clone, Debug)]
pub(crate) struct RenderFailure {
    pub(crate) helper: String,
    pub(crate) reason: String,
    pub(crate) definition: Option<SourceLocation>,
    pub(crate) underlying: String,
}

pub(crate) struct HelperRenderError {
    pub(crate) render: handlebars::RenderError,
    pub(crate) helper: Option<RenderFailure>,
}

struct RenderState {
    packs: Vec<TemplateHelperPack>,
    payload_root: std::path::PathBuf,
    runtime: Option<HelperRuntime>,
    failure: Option<RenderFailure>,
}

thread_local! {
    static ACTIVE_RENDER: RefCell<Vec<Rc<RefCell<RenderState>>>> = const { RefCell::new(Vec::new()) };
}

struct RenderScopeGuard {
    state: Rc<RefCell<RenderState>>,
}

impl Drop for RenderScopeGuard {
    fn drop(&mut self) {
        ACTIVE_RENDER.with(|active| {
            let popped = active.borrow_mut().pop();
            debug_assert!(
                popped
                    .as_ref()
                    .is_some_and(|state| Rc::ptr_eq(state, &self.state))
            );
        });
    }
}

#[derive(Clone)]
struct LuaHelperAdapter {
    name: String,
}

impl handlebars::HelperDef for LuaHelperAdapter {
    fn call_inner<'reg: 'rc, 'rc>(
        &self,
        helper: &handlebars::Helper<'rc>,
        _registry: &'reg handlebars::Handlebars<'reg>,
        _context: &'rc handlebars::Context,
        _render_context: &mut handlebars::RenderContext<'reg, 'rc>,
    ) -> std::result::Result<handlebars::ScopedJson<'rc>, handlebars::RenderError> {
        if helper.is_block() {
            return Err(handlebars::RenderErrorReason::Other(format!(
                "custom template helper `{}` is value-only and cannot be used as a block",
                self.name
            ))
            .into());
        }
        for value in helper.params().iter().chain(helper.hash().values()) {
            if value.is_value_missing() {
                return Err(handlebars::RenderError::strict_error(value.relative_path()));
            }
        }
        let result = ACTIVE_RENDER.with(|active| {
            let state = active.borrow().last().cloned().ok_or_else(|| {
                WombatError::invariant("template helper invoked outside an active render")
            })?;
            let mut state = state.borrow_mut();
            if state.runtime.is_none() {
                state.runtime = Some(HelperRuntime::new(&state.payload_root, &state.packs)?);
            }
            let positional = helper
                .params()
                .iter()
                .map(|value| json_to_frozen(value.value()))
                .collect::<Result<Vec<_>>>()?;
            let options = helper
                .hash()
                .iter()
                .map(|(name, value)| Ok(((*name).to_string(), json_to_frozen(value.value())?)))
                .collect::<Result<BTreeMap<_, _>>>()?;
            let runtime = state.runtime.as_mut().expect("helper runtime initialized");
            runtime.call(&self.name, &positional, options)
        });
        match result {
            Ok(value) => Ok(handlebars::ScopedJson::Derived(
                serde_json::to_value(value).map_err(|error| {
                    handlebars::RenderError::from(handlebars::RenderErrorReason::Other(
                        error.to_string(),
                    ))
                })?,
            )),
            Err(error) => {
                let reason = error.to_string();
                let definition = ACTIVE_RENDER.with(|active| {
                    active.borrow().last().and_then(|state| {
                        state
                            .borrow()
                            .packs
                            .iter()
                            .flat_map(|pack| &pack.exports)
                            .find(|export| export.name == self.name)
                            .map(|export| export.defined_at.clone())
                    })
                });
                let failure = RenderFailure {
                    helper: self.name.clone(),
                    reason: super::clean_lua_reason(&reason),
                    definition,
                    underlying: reason.clone(),
                };
                ACTIVE_RENDER.with(|active| {
                    if let Some(state) = active.borrow().last() {
                        state.borrow_mut().failure = Some(failure);
                    }
                });
                Err(handlebars::RenderErrorReason::Other(format!(
                    "custom template helper `{}` failed: {}",
                    self.name,
                    super::clean_lua_reason(&reason)
                ))
                .into())
            }
        }
    }
}

pub(crate) fn register_handlebars_helpers(
    renderer: &mut handlebars::Handlebars<'_>,
    packs: &[TemplateHelperPack],
) {
    for export in packs.iter().flat_map(|pack| &pack.exports) {
        renderer.register_helper(
            &export.name,
            Box::new(LuaHelperAdapter {
                name: export.name.clone(),
            }),
        );
    }
}

pub(crate) fn render(
    renderer: &handlebars::Handlebars<'_>,
    template: &str,
    context: &crate::model::frozen::FrozenValue,
    packs: &[TemplateHelperPack],
    payload_root: &Path,
) -> std::result::Result<String, Box<HelperRenderError>> {
    let state = Rc::new(RefCell::new(RenderState {
        packs: packs.to_vec(),
        payload_root: payload_root.to_path_buf(),
        runtime: None,
        failure: None,
    }));
    ACTIVE_RENDER.with(|active| active.borrow_mut().push(Rc::clone(&state)));
    let guard = RenderScopeGuard {
        state: Rc::clone(&state),
    };
    let result = renderer.render(template, context);
    drop(guard);
    result.map_err(|render| {
        Box::new(HelperRenderError {
            render,
            helper: state.borrow_mut().failure.take(),
        })
    })
}

pub(crate) fn registry_digest(packs: &[TemplateHelperPack]) -> Result<String> {
    Ok(crate::storage::digest::sha256(&serde_json::to_vec(&(
        CONTRACT_VERSION,
        packs,
    ))?))
}

fn json_to_frozen(value: &serde_json::Value) -> Result<crate::model::frozen::FrozenValue> {
    serde_json::from_value(value.clone()).map_err(WombatError::from)
}

struct PayloadLoader {
    root: std::path::PathBuf,
    allowed: BTreeMap<String, SourceFile>,
    cache: BTreeMap<String, mlua::RegistryKey>,
    loading: Vec<String>,
}

struct HelperRuntime {
    lua: Lua,
    functions: BTreeMap<String, mlua::RegistryKey>,
}

impl HelperRuntime {
    fn new(root: &Path, packs: &[TemplateHelperPack]) -> Result<Self> {
        let lua = constrained_lua()?;
        let allowed = source_catalog(packs)?;
        let loader = Rc::new(RefCell::new(PayloadLoader {
            root: root.to_path_buf(),
            allowed,
            cache: BTreeMap::new(),
            loading: Vec::new(),
        }));
        install_payload_require(&lua, Rc::clone(&loader))?;
        let mut functions = BTreeMap::new();
        for pack in packs {
            let value = load_payload_module(&lua, &loader, &pack.module)?;
            let Value::Table(exports) = value else {
                return Err(WombatError::corrupt_state(format!(
                    "frozen template helper pack `{}` no longer returns a table",
                    pack.module
                )));
            };
            for export in &pack.exports {
                let function = exports
                    .raw_get::<Function>(export.export.as_str())
                    .map_err(|error| {
                        WombatError::corrupt_state(format!(
                            "frozen template helper `{}` is unavailable: {error}",
                            export.name
                        ))
                    })?;
                functions.insert(export.name.clone(), lua.create_registry_value(function)?);
            }
        }
        Ok(Self { lua, functions })
    }

    fn call(
        &mut self,
        name: &str,
        positional: &[crate::model::frozen::FrozenValue],
        options: BTreeMap<String, crate::model::frozen::FrozenValue>,
    ) -> Result<crate::model::frozen::FrozenValue> {
        let key = self.functions.get(name).ok_or_else(|| {
            WombatError::corrupt_state(format!("frozen template helper `{name}` is unavailable"))
        })?;
        let function: Function = self.lua.registry_value(key)?;
        let mut arguments = MultiValue::new();
        for value in positional {
            arguments.push_back(value.to_lua(&self.lua)?);
        }
        arguments.push_back(crate::model::frozen::FrozenValue::Map(options).to_lua(&self.lua)?);
        let values: MultiValue = function.call(arguments)?;
        if values.len() != 1 {
            return Err(WombatError::configuration(format!(
                "template helper `{name}` must return exactly one value; returned {}",
                values.len()
            )));
        }
        let value = values.into_iter().next().expect("one helper return value");
        if value.is_nil() {
            return Err(WombatError::configuration(format!(
                "template helper `{name}` returned nil; return w.null for explicit null"
            )));
        }
        crate::model::frozen::FrozenValue::from_lua(value).map_err(|error| {
            WombatError::configuration(format!(
                "template helper `{name}` returned an invalid value: {error}"
            ))
        })
    }
}

fn install_payload_require(lua: &Lua, loader: Rc<RefCell<PayloadLoader>>) -> Result<()> {
    let api = minimal_wombat_api(lua)?;
    lua.globals().set(
        "require",
        lua.create_function(move |lua, module: String| {
            if module == "wombat" {
                return Ok(Value::Table(api.clone()));
            }
            load_payload_module(lua, &loader, &module).map_err(mlua::Error::external)
        })?,
    )?;
    Ok(())
}

fn load_payload_module(
    lua: &Lua,
    loader: &Rc<RefCell<PayloadLoader>>,
    module: &str,
) -> Result<Value> {
    let relative = helper_module_path(module)?;
    if let Some(value) = loader
        .borrow()
        .cache
        .get(module)
        .map(|key| lua.registry_value(key))
    {
        return value.map_err(WombatError::from);
    }
    if loader.borrow().loading.iter().any(|item| item == module) {
        return Err(WombatError::configuration(format!(
            "template helper module dependency cycle includes `{module}`"
        )));
    }
    let direct = format!("lua/{relative}.lua");
    let initial = format!("lua/{relative}/init.lua");
    let selected = {
        let loader = loader.borrow();
        if loader.allowed.contains_key(&direct) {
            direct
        } else if loader.allowed.contains_key(&initial) {
            initial
        } else {
            return Err(WombatError::configuration(format!(
                "template helper module `{module}` was not captured during construction; require dependencies at module initialization"
            )));
        }
    };
    let path = loader.borrow().root.join(&selected);
    let bytes = fs::read(&path).map_err(|error| WombatError::io(&path, error))?;
    let source = std::str::from_utf8(&bytes).map_err(|error| {
        WombatError::corrupt_state(format!(
            "template helper payload `{selected}` is not UTF-8: {error}"
        ))
    })?;
    loader.borrow_mut().loading.push(module.to_string());
    let result = lua
        .load(source)
        .set_name(format!("@{selected}"))
        .eval::<MultiValue>();
    loader.borrow_mut().loading.pop();
    let mut values = result.map_err(WombatError::from)?;
    let value = match values.len() {
        0 => Value::Boolean(true),
        1 => values.pop_front().expect("one module return"),
        _ => {
            return Err(WombatError::configuration(format!(
                "template helper module `{module}` must return exactly one value"
            )));
        }
    };
    let key = lua.create_registry_value(value.clone())?;
    loader.borrow_mut().cache.insert(module.to_string(), key);
    Ok(value)
}

pub(super) fn register_native(lua: &Lua, state: Rc<RefCell<RuntimeState>>) -> Result<Function> {
    Ok(
        lua.create_function(move |lua, (module, prefix): (String, String)| {
            let location = caller_location(lua, &state);
            declare(&state, module, prefix, location).map_err(mlua::Error::external)
        })?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::frozen::FrozenValue;

    #[test]
    fn templates_without_custom_calls_do_not_initialize_the_helper_runtime() {
        let location = SourceLocation {
            source: "lua/example.lua".to_string(),
            line: Some(1),
            column: None,
        };
        let packs = vec![TemplateHelperPack {
            contract_version: CONTRACT_VERSION,
            module: "example".to_string(),
            prefix: String::new(),
            exports: vec![TemplateHelperExport {
                export: "value".to_string(),
                name: "value".to_string(),
                defined_at: location.clone(),
            }],
            sources: Vec::new(),
            declared_at: SourceTrace {
                primary: location,
                callers: Vec::new(),
            },
        }];
        let mut renderer = handlebars::Handlebars::new();
        renderer.set_strict_mode(true);
        register_handlebars_helpers(&mut renderer, &packs);
        renderer
            .register_template_string("plain", "plain text")
            .unwrap();

        let rendered = match render(
            &renderer,
            "plain",
            &FrozenValue::Map(BTreeMap::new()),
            &packs,
            Path::new("this-helper-payload-does-not-exist"),
        ) {
            Ok(rendered) => rendered,
            Err(_) => panic!("a built-in-only template initialized the helper runtime"),
        };

        assert_eq!(rendered, "plain text");
    }
}
