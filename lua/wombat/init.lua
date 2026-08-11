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

local function requirement(namespace, kind, name, options, preferred)
    if type(name) ~= "string" then
        error("w." .. namespace .. "." .. kind .. "() requires a string name", 3)
    end
    if options ~= nil and type(options) ~= "table" then
        error("w." .. namespace .. "." .. kind .. "() options must be a table", 3)
    end
    return native.declare_requirement(kind, name, options, preferred)
end

wombat.need = {}
wombat.prefer = {}

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

local function validate_install(source_path, options, explicit_kind)
    if type(source_path) ~= "string" then
        error("install() requires a string source path", 2)
    end
    if options ~= nil and type(options) ~= "table" then
        error("install() options must be a table", 2)
    end
    if options ~= nil then
        if options.to ~= nil and type(options.to) ~= "string" then
            error("install() requires a string `to` target", 2)
        end
        for key in pairs(options) do
            if key ~= "to" and key ~= "with" then
                error("install() does not support option `" .. tostring(key) .. "`", 3)
            end
        end
    end

    local has_with = options ~= nil and options.with ~= nil
    local kind = explicit_kind
    if kind == nil then
        kind = (has_with or source_path:sub(-5) == ".tmpl") and "template" or "auto"
    end
    if kind == "file" and has_with then
        error("install.file() does not support `with`; use install.template()", 3)
    end
    local context = options and options.with or nil
    if kind == "template" and context == nil then
        context = {}
    end
    return native.install_path(source_path, options and options.to or nil, kind, context)
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

function wombat.module.config()
    return native.module_config()
end

return wombat
