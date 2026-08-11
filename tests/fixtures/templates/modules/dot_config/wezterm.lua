local w = require("wombat")
w.module.from(".config")
local theme = w.using("theme")

w.install.template("wezterm.lua.tmpl", {
    with = {
        colors = theme.colors,
        variants = { "dark", "light" },
    },
})
