local native = require("_wombat")

local wombat = {}

function wombat.use(name, config)
    return native.use_module(name, config)
end

function wombat.using(name)
    return native.using_module(name)
end

function wombat.install(source_path, options)
    if type(options) ~= "table" then
        error("install() requires an options table with an explicit `to` target", 2)
    end
    if type(options.to) ~= "string" then
        error("install() requires a string `to` target", 2)
    end
    for key in pairs(options) do
        if key ~= "to" then
            error("install() does not support option `" .. tostring(key) .. "` in this build", 2)
        end
    end

    return native.install_file(source_path, options.to)
end

wombat.module = {}

function wombat.module.config()
    return native.module_config()
end

return wombat
