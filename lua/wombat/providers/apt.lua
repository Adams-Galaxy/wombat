local provider = require("wombat.provider")

local default_aliases = {
    rg = "ripgrep",
    nvim = "neovim",
}

local function require_map(value, label)
    if type(value) ~= "table" then
        error(label .. " must be a table")
    end
    return value
end

local function normalized_strings(value, label, allow_empty)
    require_map(value, label)
    local result = {}
    for index, item in ipairs(value) do
        if type(item) ~= "string" or item == "" or item:find("[%s\r\n]") then
            error(label .. " entries must be non-empty tokens")
        end
        result[index] = item
    end
    for key, _ in pairs(value) do
        if type(key) ~= "number" or key < 1 or key % 1 ~= 0 or key > #result then
            error(label .. " must be an array")
        end
    end
    if not allow_empty and #result == 0 then
        error(label .. " must not be empty")
    end
    table.sort(result)
    local unique = {}
    for _, item in ipairs(result) do
        if unique[#unique] ~= item then table.insert(unique, item) end
    end
    return unique
end

local function validate_source_name(name)
    if type(name) ~= "string" or not name:match("^[a-z][a-z0-9_-]*$") or #name > 64 then
        error("Apt source names must start with a lowercase letter and contain at most 64 lowercase letters, digits, `_`, or `-`")
    end
end

local function validate_url(value, label)
    if type(value) ~= "string" or value:find("%s") or not value:match("^https?://[^/]+") then
        error(label .. " must be an HTTP or HTTPS URL")
    end
end

local function normalize_source(name, value)
    validate_source_name(name)
    local source = require_map(value, "Apt source `" .. name .. "`")
    for key, _ in pairs(source) do
        if key ~= "uri" and key ~= "suite" and key ~= "components" and key ~= "architectures"
            and key ~= "key" and key ~= "replace" then
            error("Apt source `" .. name .. "` does not support `" .. tostring(key) .. "`")
        end
    end
    validate_url(source.uri, "Apt source `" .. name .. "` uri")
    if type(source.suite) ~= "string" or source.suite == "" or source.suite:find("[%s\r\n]") then
        error("Apt source `" .. name .. "` suite must be a non-empty token")
    end
    local components = normalized_strings(source.components, "Apt source `" .. name .. "` components", false)
    local architectures
    if source.architectures ~= nil then
        architectures = normalized_strings(
            source.architectures,
            "Apt source `" .. name .. "` architectures",
            false
        )
    end
    local key = require_map(source.key, "Apt source `" .. name .. "` key")
    for field, _ in pairs(key) do
        if field ~= "url" and field ~= "format" and field ~= "sha256" then
            error("Apt source `" .. name .. "` key does not support `" .. tostring(field) .. "`")
        end
    end
    validate_url(key.url, "Apt source `" .. name .. "` key url")
    local format = key.format or "gpg"
    if format ~= "gpg" and format ~= "asc" then
        error("Apt source `" .. name .. "` key format must be `gpg` or `asc`")
    end
    if key.sha256 ~= nil
        and (type(key.sha256) ~= "string" or #key.sha256 ~= 64 or key.sha256:find("[^0-9a-fA-F]")) then
        error("Apt source `" .. name .. "` key sha256 must be 64 hexadecimal digits")
    end
    if key.sha256 == nil and not key.url:match("^https://") then
        error("Apt source `" .. name .. "` key requires HTTPS unless sha256 is supplied")
    end
    if source.replace ~= nil and type(source.replace) ~= "boolean" then
        error("Apt source `" .. name .. "` replace must be boolean")
    end
    local normalized = {
        name = name,
        uri = source.uri,
        suite = source.suite,
        components = components,
        key = {
            url = key.url,
            format = format,
            sha256 = key.sha256 and key.sha256:lower() or nil,
        },
        replace = source.replace == true,
    }
    if architectures ~= nil then normalized.architectures = architectures end
    return normalized
end

local function normalize_config(config)
    for key, _ in pairs(config) do
        if key ~= "update" and key ~= "aliases" and key ~= "sources" then
            error("Apt provider does not support `with." .. key .. "`")
        end
    end
    if config.update ~= nil and type(config.update) ~= "boolean" then
        error("Apt provider `with.update` must be boolean")
    end
    local sources = config.sources or {}
    require_map(sources, "Apt provider `with.sources`")
    local normalized = {}
    for name, source in pairs(sources) do
        validate_source_name(name)
        normalized[name] = normalize_source(name, source)
    end
    return normalized
end

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
        local sources = normalize_config(config)

        local package = candidate.name
        local commands = {}
        if candidate.kind == "command" then
            if candidate.name == "fd" or candidate.name == "fd-find" then
                return provider.unsupported("Debian's fd-find package publishes fdfind, not the requested command")
            end
            commands = { candidate.name }
            local aliases = config.aliases or {}
            package = aliases[candidate.name] or default_aliases[candidate.name] or candidate.name
        elseif candidate.kind == "package" then
            if candidate.provider ~= nil and candidate.provider ~= "apt" then
                return provider.unsupported("package explicitly requests another provider")
            end
            local options = candidate.with or {}
            package = options.name or candidate.name
            commands = candidate.publications.commands or {}
            for key, _ in pairs(options) do
                if key ~= "name" and key ~= "source" then
                    error("Apt package does not support `with." .. key .. "`")
                end
            end
            if options.source ~= nil then
                if type(options.source) ~= "string" or sources[options.source] == nil then
                    error("Apt package source must name an entry from provider `with.sources`")
                end
            end
        else
            error("Apt provider received unknown candidate kind `" .. tostring(candidate.kind) .. "`")
        end
        return provider.binding({
            identity = "package:" .. package,
            elevated = true,
            package = package,
            publications = { commands = commands },
            prerequisites = candidate.kind == "package" and candidate.with.source ~= nil
                and { "source:" .. candidate.with.source } or {},
            data = {
                name = package,
                source = candidate.kind == "package" and candidate.with.source or nil,
            },
        })
    end,

    plan = function(bindings, _, config)
        local sources = normalize_config(config)
        local referenced = {}
        for _, binding in ipairs(bindings) do
            if binding.data.source ~= nil then
                referenced[binding.data.source] = true
            end
        end
        local names = {}
        for name, _ in pairs(referenced) do table.insert(names, name) end
        table.sort(names)
        local planned = {}
        for _, name in ipairs(names) do
            table.insert(planned, provider.prerequisite({
                identity = "source:" .. name,
                description = "Configure Apt source " .. name,
                elevated = true,
                data = sources[name],
            }))
        end
        if (config.update == true and #bindings > 0) or #names > 0 then
            table.insert(planned, provider.operation({
                identity = "update-index",
                description = "Update the Apt package index",
                elevated = true,
                data = { forced = config.update == true },
            }))
        end
        return planned
    end,
})
