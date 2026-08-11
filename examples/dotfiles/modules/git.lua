local w = require("wombat")
local config = w.module.config()

w.module.from(".")
w.install(".gitconfig", { with = config })
