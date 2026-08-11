local provider = require("wombat.provider")

local default_aliases = {
    rg = "ripgrep",
    nvim = "neovim",
}

local function debian_family(target)
    if target.os.name ~= "linux" or target.os.distribution == nil then
        return false
    end
    local distribution = target.os.distribution
    if distribution.id == "debian" or distribution.id == "ubuntu" then
        return true
    end
    for _, value in ipairs(distribution.id_like or {}) do
        if value == "debian" then
            return true
        end
    end
    return false
end

return provider.define({
    resolve = function(candidate, target, config)
        if not debian_family(target) then
            return provider.unsupported("Apt provider requires a Debian-family Linux target")
        end
        for key, _ in pairs(config) do
            if key ~= "update" and key ~= "aliases" then
                error("Apt provider does not support `with." .. key .. "`")
            end
        end
        if config.update ~= nil and type(config.update) ~= "boolean" then
            error("Apt provider `with.update` must be boolean")
        end

        local package = candidate.name
        local commands = {}
        if candidate.kind == "command" then
            if candidate.name == "fd" or candidate.name == "fd-find" then
                return provider.unsupported("Debian's fd-find package publishes fdfind, not the requested command")
            end
            commands = { candidate.name }
            local aliases = config.aliases or {}
            package = aliases[candidate.name] or default_aliases[candidate.name] or candidate.name
        else
            if candidate.provider ~= "apt" then
                return provider.unsupported("package explicitly requests another provider")
            end
            local options = candidate.with or {}
            package = options.name or candidate.name
            commands = candidate.publications.commands or {}
            for key, _ in pairs(options) do
                if key ~= "name" then
                    error("Apt package does not support `with." .. key .. "`")
                end
            end
        end
        return provider.binding({
            identity = "package:" .. package,
            package = package,
            publications = { commands = commands },
            data = { name = package },
        })
    end,

    plan = function(bindings, _, config)
        if config.update == true and #bindings > 0 then
            return {
                provider.operation({
                    identity = "update-index",
                    description = "Update the Apt package index",
                    elevated = true,
                    data = {},
                }),
            }
        end
        return {}
    end,
})
