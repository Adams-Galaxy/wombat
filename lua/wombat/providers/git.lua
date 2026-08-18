local provider = require("wombat.provider")

return provider.define({
    resolve = function(candidate, target, config)
        for key, _ in pairs(config) do
            error("Git provider does not support `with." .. key .. "`")
        end
        if candidate.kind ~= "package" then
            return provider.unsupported("Git only resolves package candidates")
        end
        if candidate.provider ~= nil and candidate.provider ~= "git" then
            return provider.unsupported("package explicitly requests another provider")
        end

        local options = candidate.with or {}
        if type(options.repository) ~= "string" or options.repository == "" then
            error("Git package requires `with.repository`")
        end
        if type(options.to) ~= "string" or options.to:sub(1, 1) ~= "/" then
            error("Git package requires an absolute `with.to`")
        end
        if options.ref ~= nil and type(options.ref) ~= "string" then
            error("Git package `with.ref` must be a string")
        end
        for key, _ in pairs(options) do
            if key ~= "repository" and key ~= "to" and key ~= "ref" then
                error("Git package does not support `with." .. key .. "`")
            end
        end

        local commands = candidate.publications.commands or {}
        return provider.binding({
            identity = "git:" .. options.to,
            elevated = false,
            publications = { commands = commands },
            data = { repository = options.repository, to = options.to, ref = options.ref },
        })
    end,
})
