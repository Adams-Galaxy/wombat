local w = require("wombat")
local palette = require("themes.kanagawa")

w.use("consumer")
w.use("prompt")
w.use("theme", {
    name = palette.name,
    colors = palette.colors,
})
w.use("theme")
w.use("theme", {
    colors = palette.colors,
    name = palette.name,
})
