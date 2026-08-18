local provider = require("wombat.provider")

local FLATHUB_DESCRIPTOR = "https://dl.flathub.org/repo/flathub.flatpakrepo"
local FLATHUB_URL = "https://dl.flathub.org/repo/"

local function normalize_scope(value, label)
    local scope = value or "system"
    if scope ~= "system" and scope ~= "user" then
        error(label .. " must be `system` or `user`")
    end
    return scope
end

local function normalize_config(config)
    for key, _ in pairs(config) do
        if key ~= "scope" then
            error("Flatpak provider does not support `with." .. tostring(key) .. "`")
        end
    end
    return normalize_scope(config.scope, "Flatpak provider `with.scope`")
end

return provider.define({
    resolve = function(candidate, target, config)
        local default_scope = normalize_config(config)
        if target.os.name ~= "linux" then
            return provider.unsupported("Flatpak provider requires a Linux target")
        end
        if candidate.kind ~= "package" then
            return provider.unsupported("Flatpak resolves explicit package requirements only")
        end
        if candidate.provider ~= nil and candidate.provider ~= "flatpak" then
            return provider.unsupported("package explicitly requests another provider")
        end
        if candidate.minimum ~= nil then
            error("Flatpak packages do not support minimum versions; select an explicit branch instead")
        end

        local options = candidate.with or {}
        for key, _ in pairs(options) do
            if key ~= "remote" and key ~= "kind" and key ~= "branch" and key ~= "scope" then
                error("Flatpak package does not support `with." .. tostring(key) .. "`")
            end
        end
        local remote = options.remote or "flathub"
        if remote ~= "flathub" then
            error("Flatpak currently supports only the `flathub` remote")
        end
        local kind = options.kind or "app"
        if kind ~= "app" and kind ~= "runtime" then
            error("Flatpak package `with.kind` must be `app` or `runtime`")
        end
        local scope = normalize_scope(options.scope or default_scope, "Flatpak package `with.scope`")
        if options.branch ~= nil
            and (type(options.branch) ~= "string"
                or not options.branch:match("^[A-Za-z0-9][A-Za-z0-9_.-]*$")) then
            error("Flatpak package `with.branch` must be a safe branch token")
        end
        local id = candidate.name
        if type(id) ~= "string"
            or not id:match("^[A-Za-z0-9][A-Za-z0-9_.-]*%.[A-Za-z0-9_.-]+$") then
            error("Flatpak package name must be an application or runtime ID")
        end
        local branch = options.branch or "current"
        local identity = table.concat({ scope, kind, id, target.arch, branch }, ":")
        return provider.binding({
            identity = "ref:" .. identity,
            elevated = scope == "system",
            publications = { commands = candidate.publications.commands or {} },
            prerequisites = { "remote:" .. scope .. ":flathub" },
            data = {
                id = id,
                remote = remote,
                kind = kind,
                scope = scope,
                arch = target.arch,
                branch = options.branch,
            },
        })
    end,

    plan = function(bindings, _, config)
        normalize_config(config)
        local scopes = {}
        for _, binding in ipairs(bindings) do scopes[binding.data.scope] = true end
        local planned = {}
        for _, scope in ipairs({ "system", "user" }) do
            if scopes[scope] then
                table.insert(planned, provider.prerequisite({
                    identity = "remote:" .. scope .. ":flathub",
                    description = "Configure the " .. scope .. " Flathub remote",
                    elevated = scope == "system",
                    data = {
                        name = "flathub",
                        scope = scope,
                        descriptor = FLATHUB_DESCRIPTOR,
                        url = FLATHUB_URL,
                    },
                }))
            end
        end
        return planned
    end,
})
