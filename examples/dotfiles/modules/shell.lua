local w = require("wombat")
local theme = w.using("theme")
local config = w.module.config()

w.module.from(".")
w.install(".zshrc", {
    with = {
        theme = theme.name,
        search = config.search,
    },
})
