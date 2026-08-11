local w = require("wombat")

w.providers({
    { name = "brew", with = { aliases = { fd = "fd" } } },
})

local search = w.prefer.command("rg", {
    minimum = "14.0.0",
    accept = {
        { name = "grep" },
    },
})

w.need.package("visual-studio-code", {
    provider = "brew",
    publishes = { commands = { "code" } },
    with = { kind = "cask" },
})

assert(search.provider == "brew")
assert(search.name == "rg")
assert(search.package == "ripgrep")
assert(not pcall(function() search.provider = "company" end))
