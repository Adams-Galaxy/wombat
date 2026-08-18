local provider = require("wombat.provider")

local default_aliases = {
    rg = "ripgrep",
    nvim = "neovim",
}

local function fedora(target)
    return target.os.name == "linux"
        and target.os.distribution ~= nil
        and target.os.distribution.id == "fedora"
end

local function normalize_config(config)
    for key, _ in pairs(config) do
        if key ~= "aliases" then
            error("DNF provider does not support `with." .. tostring(key) .. "`")
        end
    end
    local aliases = config.aliases or {}
    if type(aliases) ~= "table" then
        error("DNF provider `with.aliases` must be a table")
    end
    for command, package in pairs(aliases) do
        if type(command) ~= "string" or command == "" or type(package) ~= "string" or package == "" then
            error("DNF provider aliases must map non-empty command names to non-empty package names")
        end
    end
end

local function rpmfusion_prerequisites(policy)
    if policy == nil then return {} end
    if policy == "free" then return { "repository:rpmfusion-free" } end
    if policy == "nonfree" then
        return { "repository:rpmfusion-free", "repository:rpmfusion-nonfree" }
    end
    error("DNF package `with.rpmfusion` must be `free` or `nonfree`")
end

return provider.define({
    resolve = function(candidate, target, config)
        normalize_config(config)
        if not fedora(target) then
            return provider.unsupported("DNF provider requires a Fedora Linux target")
        end

        local package = candidate.name
        local commands = {}
        local rpmfusion
        if candidate.kind == "command" then
            commands = { candidate.name }
            package = (config.aliases or {})[candidate.name]
                or default_aliases[candidate.name]
                or candidate.name
        elseif candidate.kind == "package" then
            if candidate.provider ~= nil and candidate.provider ~= "dnf" then
                return provider.unsupported("package explicitly requests another provider")
            end
            local options = candidate.with or {}
            for key, _ in pairs(options) do
                if key ~= "name" and key ~= "rpmfusion" then
                    error("DNF package does not support `with." .. tostring(key) .. "`")
                end
            end
            package = options.name or candidate.name
            rpmfusion = options.rpmfusion
            commands = candidate.publications.commands or {}
        else
            error("DNF provider received unknown candidate kind `" .. tostring(candidate.kind) .. "`")
        end

        if type(package) ~= "string" or not package:match("^[%w][%w+._-]*$") then
            error("DNF package name must be a package token beginning with a letter or number")
        end
        local prerequisites = rpmfusion_prerequisites(rpmfusion)
        return provider.binding({
            identity = "package:" .. package,
            package = package,
            elevated = true,
            publications = { commands = commands },
            prerequisites = prerequisites,
            data = { name = package, rpmfusion = rpmfusion },
        })
    end,

    plan = function(bindings, target, config)
        normalize_config(config)
        local required = {}
        for _, binding in ipairs(bindings) do
            for _, identity in ipairs(binding.prerequisites or {}) do
                required[identity] = true
            end
        end
        if next(required) == nil then return {} end
        local distribution = target.os.distribution
        local major = distribution and distribution.version and distribution.version.major
        if type(major) ~= "number" or major < 1 or major % 1 ~= 0 then
            error("RPM Fusion requires a numeric Fedora major version")
        end
        local planned = {}
        for _, kind in ipairs({ "free", "nonfree" }) do
            local identity = "repository:rpmfusion-" .. kind
            if required[identity] then
                table.insert(planned, provider.prerequisite({
                    identity = identity,
                    description = "Configure RPM Fusion " .. kind .. " for Fedora " .. tostring(major),
                    elevated = true,
                    data = {
                        kind = kind,
                        major = major,
                        package = "rpmfusion-" .. kind .. "-release",
                        url = "https://mirrors.rpmfusion.org/" .. kind .. "/fedora/rpmfusion-"
                            .. kind .. "-release-" .. tostring(major) .. ".noarch.rpm",
                    },
                }))
            end
        end
        return planned
    end,
})
