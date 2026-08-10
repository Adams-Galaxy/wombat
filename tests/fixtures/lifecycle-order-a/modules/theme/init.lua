local w = require("wombat")

_G.theme_evaluations = (_G.theme_evaluations or 0) + 1
assert(_G.theme_evaluations == 1, "theme evaluated more than once")

return w.module.config()
