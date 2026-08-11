local w = require("wombat")
w.module.from(".config")

local theme = w.using("theme")
assert(theme.colors.accent == "blue")

w.install("wombat/prompt.txt")

return {
    observed_theme = theme.name,
}
