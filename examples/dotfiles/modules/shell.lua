local w = require("wombat")
local theme = w.using("theme")
local config = w.module.config()

w.module.from(".")
w.install(".zshrc", {
    with = w.template.context({
        theme = theme.name,
        search = config.search,
        os = w.os,
        arch = w.arch,
        paths = w.paths,
    }),
})
