local provider = require("wombat.provider")

local default_aliases = {
    rg = "ripgrep",
    nvim = "neovim",
}

return provider.define({
    resolve = function(candidate, target, config)
        if target.os.name ~= "macos" then
            return provider.unsupported("Homebrew provider requires a macOS target")
        end

        local package = candidate.name
        local package_kind = "formula"
        local commands = {}
        if candidate.kind == "command" then
            commands = { candidate.name }
            local aliases = config.aliases or {}
            package = aliases[candidate.name] or default_aliases[candidate.name] or candidate.name
        elseif candidate.kind == "package" then
            if candidate.provider ~= nil and candidate.provider ~= "brew" then
                return provider.unsupported("package explicitly requests another provider")
            end
            local options = candidate.with or {}
            package = options.name or candidate.name
            package_kind = options.kind or "formula"
            commands = candidate.publications.commands or {}
            for key, _ in pairs(options) do
                if key ~= "name" and key ~= "kind" then
                    error("Homebrew package does not support `with." .. key .. "`")
                end
            end
        else
            error("Homebrew provider received unknown candidate kind `" .. tostring(candidate.kind) .. "`")
        end
        if package_kind ~= "formula" and package_kind ~= "cask" then
            error("Homebrew package `with.kind` must be `formula` or `cask`")
        end
        return provider.binding({
            identity = package_kind .. ":" .. package,
            elevated = false,
            package = package,
            publications = { commands = commands },
            data = { kind = package_kind, name = package },
        })
    end,
})
