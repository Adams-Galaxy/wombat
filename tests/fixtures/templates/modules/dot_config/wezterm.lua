local w = require("wombat")
local theme = w.using("theme")

w.install.template("wezterm.lua.tmpl", {
    with = {
        colors = theme.colors,
        variants = { "dark", "light" },
    },
})
