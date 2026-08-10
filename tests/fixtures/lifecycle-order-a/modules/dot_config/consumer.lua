local w = require("wombat")

w.use("helper")

local theme = w.using("theme")
assert(theme.name == "kanagawa")
theme.colors.accent = "mutated"

local fresh_theme = w.using("theme")
assert(fresh_theme.colors.accent == "blue", "using() shared a mutable export")

w.install("wombat/consumer.txt")

return {
    observed_theme = fresh_theme.name,
}
