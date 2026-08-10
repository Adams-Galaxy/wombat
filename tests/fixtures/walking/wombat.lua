local w = require("wombat")

assert(_VERSION == "Lua 5.5")

w.install("starship.toml", {
    to = "~/.config/starship.toml",
})
