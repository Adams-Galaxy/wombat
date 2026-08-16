local native = require("_wombat")

local wombat = {}

wombat.input = {}

local function input_spec(kind, options)
    if options == nil then
        options = {}
    elseif type(options) ~= "table" then
        error("w.input." .. kind .. "() options must be a table", 3)
    end
    return native.input_spec(kind, options)
end

function wombat.input.flag(options)
    return input_spec("flag", options)
end

function wombat.input.choice(options)
    return input_spec("choice", options)
end

function wombat.input.string(options)
    return input_spec("string", options)
end

function wombat.input.integer(options)
    return input_spec("integer", options)
end

function wombat.input.target(options)
    return input_spec("target", options)
end

function wombat.inputs(schema)
    if type(schema) ~= "table" then
        error("w.inputs() requires a schema table", 2)
    end
    return native.resolve_inputs(schema)
end

wombat.host = native.host_context()
wombat.target = native.target_context()
wombat.os = native.common_os_context()
wombat.paths = native.paths_context()
wombat.null = native.null

function wombat.array(value)
    if value ~= nil and type(value) ~= "table" then
        error("w.array() requires a table when a value is provided", 2)
    end
    return native.array(value)
end

wombat.json = {}

function wombat.json.decode(path)
    if type(path) ~= "string" then error("w.json.decode() requires a string path", 2) end
    return native.json_decode(path)
end

function wombat.json.encode(value)
    return native.json_encode(value)
end

wombat.toml = {}

function wombat.toml.decode(path)
    if type(path) ~= "string" then error("w.toml.decode() requires a string path", 2) end
    return native.toml_decode(path)
end

function wombat.toml.encode(value)
    return native.toml_encode(value)
end

wombat.yaml = {}

function wombat.yaml.decode(path)
    if type(path) ~= "string" then error("w.yaml.decode() requires a string path", 2) end
    return native.yaml_decode(path)
end

function wombat.yaml.encode(value)
    return native.yaml_encode(value)
end

wombat.template = {}

function wombat.template.context(value)
    if type(value) ~= "table" then
        error("w.template.context() requires a table", 2)
    end
    return native.template_context(value)
end

function wombat.template.helpers(module, options)
    if type(module) ~= "string" then
        error("w.template.helpers() requires a string module name", 2)
    end
    if options ~= nil and type(options) ~= "table" then
        error("w.template.helpers() options must be a table", 2)
    end
    options = options or {}
    for key in pairs(options) do
        if key ~= "prefix" then
            error("w.template.helpers() does not support option `" .. tostring(key) .. "`", 2)
        end
    end
    local prefix = options.prefix or ""
    if type(prefix) ~= "string" then
        error("w.template.helpers() `prefix` must be a string", 2)
    end
    return native.declare_template_helpers(module, prefix)
end

function wombat.exec(argv, options)
    if type(argv) ~= "table" then error("w.exec() requires an argv array", 2) end
    if options ~= nil and type(options) ~= "table" then error("w.exec() options must be a table", 2) end
    return native.exec(argv, options)
end

function wombat.shell(command, options)
    if type(command) ~= "string" then error("w.shell() requires a command string", 2) end
    if options ~= nil and type(options) ~= "table" then error("w.shell() options must be a table", 2) end
    return native.shell(command, options)
end

wombat.log = {}
for _, level in ipairs({ "debug", "info", "notice", "warn", "error" }) do
    wombat.log[level] = function(message, fields)
        if type(message) ~= "string" then error("w.log." .. level .. "() requires a string message", 2) end
        if fields ~= nil and type(fields) ~= "table" then error("w.log." .. level .. "() fields must be a table", 2) end
        return native.log(level, message, fields)
    end
end

function wombat.use(name, config)
    return native.use_module(name, config)
end

function wombat.using(name)
    return native.using_module(name)
end

function wombat.providers(entries)
    if type(entries) ~= "table" then
        error("w.providers() requires an array", 2)
    end
    return native.configure_providers(entries)
end

local rung_ids = setmetatable({}, { __mode = "k" })
local rung_nodes = setmetatable({}, { __mode = "k" })
local function normalize_rung_options(options)
    if options ~= nil and rung_ids[options.at] ~= nil then
        local normalized = {}
        for key, value in pairs(options) do normalized[key] = value end
        normalized.at = rung_ids[options.at]
        options = normalized
    end
    if options ~= nil and rung_ids[options.when] ~= nil then
        local normalized = {}
        for key, value in pairs(options) do normalized[key] = value end
        normalized.when = rung_ids[options.when]
        options = normalized
    end
    return options
end
local function requirement(namespace, kind, name, options, preferred)
    if type(name) ~= "string" then
        error("w." .. namespace .. "." .. kind .. "() requires a string name", 3)
    end
    if options ~= nil and type(options) ~= "table" then
        error("w." .. namespace .. "." .. kind .. "() options must be a table", 3)
    end
    options = normalize_rung_options(options)
    return native.declare_requirement(kind, name, options, preferred)
end

wombat.need = {}
wombat.prefer = {}
wombat.build = {}
local function rung_handle(id, children, core)
    local handle = setmetatable({}, {
        __newindex = function() error("w.rungs handles are immutable", 2) end,
        __tostring = function() return id end,
        __metatable = false,
    })
    rung_ids[handle] = id
    rung_nodes[handle] = { id = id, children = children or {}, core = core or false }
    return handle
end
local function readonly(entries)
    return setmetatable({}, {
        -- A missing key would otherwise read as nil, and a nil `at` or `when`
        -- means "unspecified", so a typo would silently move an action to the
        -- default rung instead of failing.
        __index = function(_, key)
            local value = entries[key]
            if value == nil then
                error("unknown rung handle `" .. tostring(key) .. "`", 2)
            end
            return value
        end,
        __newindex = function() error("w.rungs handles are immutable", 2) end,
        __metatable = false,
    })
end

wombat.rungs = readonly({
    materialise = readonly({
        before = rung_handle("materialise.before", nil, true),
        tasks = rung_handle("materialise.tasks", nil, true),
        artifacts = rung_handle("materialise.artifacts", nil, true),
        publish = rung_handle("materialise.publish", nil, true),
        after = rung_handle("materialise.after", nil, true),
    }),
    deploy = readonly({
        before = rung_handle("deploy.before", nil, true),
        apply = rung_handle("deploy.apply", nil, true),
        after = rung_handle("deploy.after", nil, true),
    }),
})

local function valid_rung_name(name)
    return type(name) == "string" and name:match("^[%w_-]+$") ~= nil
end

function wombat.rung(name, children)
    if not valid_rung_name(name) then
        error("w.rung() name must contain only ASCII letters, numbers, `-`, or `_`", 2)
    end
    if children == nil then children = {} end
    if type(children) ~= "table" then error("w.rung() children must be an array", 2) end
    local adopted = {}
    for index, child in ipairs(children) do
        local node = rung_nodes[child]
        if node == nil or node.core then error("w.rung() children must be custom rung handles", 2) end
        local function prefix(value, prefix)
            rung_ids[value] = prefix .. "." .. rung_nodes[value].id:match("[^.]+$")
            rung_nodes[value].id = rung_ids[value]
            for _, nested in ipairs(rung_nodes[value].children) do prefix(nested, rung_ids[value]) end
        end
        prefix(child, name)
        adopted[index] = child
    end
    return rung_handle(name, adopted, false)
end

local function serialize_rung(handle, seen)
    local node = rung_nodes[handle]
    if node == nil then error("w.ladder() entries must be rung handles", 3) end
    if seen[handle] then error("w.ladder() cannot reuse a rung handle", 3) end
    seen[handle] = true
    local children = {}
    for index, child in ipairs(node.children) do children[index] = serialize_rung(child, seen) end
    return { id = node.id, children = children }
end

function wombat.ladder(name, rungs)
    if not valid_rung_name(name) then error("w.ladder() requires a valid name", 2) end
    if type(rungs) ~= "table" then error("w.ladder() requires a rung array", 2) end
    local serialized, seen = {}, {}
    for index, rung in ipairs(rungs) do serialized[index] = serialize_rung(rung, seen) end
    return native.declare_ladder(name, serialized)
end

function wombat.need.command(name, options)
    return requirement("need", "command", name, options, false)
end

function wombat.need.package(name, options)
    return requirement("need", "package", name, options, false)
end

function wombat.prefer.command(name, options)
    return requirement("prefer", "command", name, options, true)
end

function wombat.prefer.package(name, options)
    return requirement("prefer", "package", name, options, true)
end

function wombat.build.task(entrypoint, params, options)
    if type(entrypoint) ~= "string" then
        error("w.build.task() requires a string entrypoint", 2)
    end
    if params ~= nil and type(params) ~= "table" then
        error("w.build.task() params must be a table", 2)
    end
    if options ~= nil and type(options) ~= "table" then
        error("w.build.task() options must be a table", 2)
    end
    return native.declare_task(entrypoint, params or {}, normalize_rung_options(options or {}))
end

function wombat.script(entrypoint, params, options)
    if type(entrypoint) ~= "string" then error("w.script() requires a string entrypoint", 2) end
    if params ~= nil and type(params) ~= "table" then error("w.script() params must be a table", 2) end
    if options ~= nil and type(options) ~= "table" then error("w.script() options must be a table", 2) end
    return native.declare_script(entrypoint, params or {}, normalize_rung_options(options or {}))
end

function wombat.generate(name, options)
    if type(name) ~= "string" then
        error("w.generate() requires a string name", 2)
    end
    if type(options) ~= "table" then
        error("w.generate() requires an options table", 2)
    end
    return native.declare_generated(name, options)
end

local function source_name(source)
    if type(source) == "string" then return source end
    if type(source) == "table" and type(source.__wombat_hidden) == "string" then
        return source.__wombat_hidden
    end
    error("install() requires a string source or w.hidden() value", 3)
end

local function validate_install(source_path, options, explicit_kind)
    local declared_source = source_name(source_path)
    if type(source_path) ~= "string" and type(source_path) ~= "table" then
        error("install() requires a source selector", 2)
    end
    if options ~= nil and type(options) ~= "table" then
        error("install() options must be a table", 2)
    end
    if options ~= nil then
        if options.to ~= nil and type(options.to) ~= "string" then
            error("install() requires a string `to` target", 2)
        end
        for key in pairs(options) do
            if key ~= "to" and key ~= "with" and key ~= "exclude" and key ~= "allow_empty" then
                error("install() does not support option `" .. tostring(key) .. "`", 3)
            end
        end
    end

    local has_with = options ~= nil and options.with ~= nil
    local kind = explicit_kind
    if kind == nil then
        kind = declared_source:sub(-5) == ".tmpl" and "template" or "auto"
    end
    if kind == "file" and has_with then
        error("install.file() does not support `with`; use install.template()", 3)
    end
    local context = options and options.with or nil
    if kind == "template" and context == nil then
        context = {}
    end
    local exclusions = options and options.exclude or {}
    if type(exclusions) == "string" then exclusions = { exclusions } end
    if type(exclusions) ~= "table" then error("install() `exclude` must be a string or array", 2) end
    local allow_empty = options and options.allow_empty or false
    if type(allow_empty) ~= "boolean" then error("install() `allow_empty` must be boolean", 2) end
    return native.install_path(source_path, options and options.to or nil, kind, context, exclusions, allow_empty)
end

local install = {}
setmetatable(install, {
    __call = function(_, source_path, options)
        return validate_install(source_path, options, nil)
    end,
})

function install.file(source_path, options)
    return validate_install(source_path, options, "file")
end

function install.template(source_path, options)
    return validate_install(source_path, options, "template")
end


wombat.install = install

wombat.module = {}

function wombat.hidden(source)
    if type(source) ~= "string" then error("w.hidden() requires a string source path", 2) end
    return native.hidden_source(source)
end

function wombat.module.from(source, options)
    source_name(source)
    if options ~= nil and type(options) ~= "table" then error("w.module.from() options must be a table", 2) end
    if options ~= nil then
        for key in pairs(options) do
            if key ~= "to" then error("w.module.from() does not support option `" .. tostring(key) .. "`", 2) end
        end
    end
    return native.module_from(source, options and options.to or nil)
end

function wombat.module.config()
    return native.module_config()
end

local common_values = { arch = true, macos = true, linux = true, wsl = true }
setmetatable(wombat, {
    __index = function(_, key)
        if common_values[key] then return native.common_value(key) end
        return nil
    end,
    __newindex = function(table, key, value)
        if common_values[key] then
            error("w." .. tostring(key) .. " is immutable", 2)
        end
        rawset(table, key, value)
    end,
})

return wombat
