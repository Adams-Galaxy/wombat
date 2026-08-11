local w = require("wombat")

w.providers({ "company" })

local tool = w.need.package("company-tool", {
    provider = "company",
    minimum = "2.0.0",
    publishes = { commands = { "company-tool" } },
    with = { channel = "stable" },
})

assert(tool.provider == "company")
assert(tool.package == "company-tool-stable")
