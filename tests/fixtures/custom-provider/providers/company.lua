local provider = require("wombat.provider")
local naming = require("naming")

assert(io == nil and os == nil and package == nil and debug == nil)
assert(dofile == nil and load == nil and loadfile == nil)

return provider.define({
    resolve = function(candidate, _target, config)
        if candidate.kind ~= "package" or candidate.provider ~= "company" then
            return provider.unsupported("company resolves explicit company packages only")
        end
        local channel = candidate.with.channel or config.channel or "stable"
        local package = naming.package(candidate.name, channel)
        return provider.binding({
            identity = "company:" .. package,
            package = package,
            publications = candidate.publications,
            data = { package = package },
        })
    end,

    check = function(ctx, binding)
        if ctx:which(binding.package) then
            return provider.satisfied("published command is available")
        end
        return provider.missing("published command is absent")
    end,

    reconcile = function(_ctx, _binding, _observation)
        error("fixture reconciliation is intentionally unavailable")
    end,
})
