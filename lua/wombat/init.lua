local native = require("_wombat")

local wombat = {}

function wombat.use(name, config)
    return native.use_module(name, config)
end

function wombat.using(name)
    return native.using_module(name)
end

function wombat.install(source_path, options)
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
            if key ~= "to" then
                error("install() does not support option `" .. tostring(key) .. "` in this build", 2)
            end
        end
    end

    return native.install_file(source_path, options and options.to or nil)
end

wombat.module = {}

function wombat.module.config()
    return native.module_config()
end

return wombat
