local w = require("wombat")

local theme = w.using("theme")
assert(theme.colors.accent == "blue")

w.install("prompt.txt", {
    to = "~/.config/wombat/prompt.txt",
})

return {
    observed_theme = theme.name,
}
