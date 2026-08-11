local w = require("wombat")
w.module.from(".config")

w.install("starship-work.toml", {
    to = ".config/starship-alt.toml",
})
