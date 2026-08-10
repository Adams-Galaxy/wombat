local w = require("wombat")
local palette = require("themes.kanagawa")

w.use("theme", {
    colors = palette.colors,
    name = palette.name,
})
w.use("theme")
w.use("theme", {
    name = palette.name,
    colors = palette.colors,
})
w.use("prompt")
w.use("consumer")
