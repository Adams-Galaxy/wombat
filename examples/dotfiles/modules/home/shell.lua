local w = require("wombat")
local theme = w.using("theme")
local config = w.module.config()

w.install(".zshrc.tmpl", {
    with = {
        theme = theme.name,
        search = config.search,
    },
})
