local w = require("wombat")
w.module.from(".config")
local theme = w.using("theme")
local enabled = true
local shell = "zsh"
local prompt

if enabled then
    if shell == "zsh" or shell == "fish" then
        prompt = "wombat"
    else
        prompt = "fallback"
    end
end

w.install("starship.toml.tmpl", {
    with = {
        colors = theme.colors,
        prompt = prompt,
    },
})
