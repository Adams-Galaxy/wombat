local w = require("wombat")
local config = w.module.config()

w.install(".gitconfig.tmpl", { with = config })
