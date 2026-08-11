local w = require("wombat")
local config = w.module.config()

if config.name == "light" then
    return { name = "light", foreground = "#3c3836", background = "#fbf1c7" }
end

return { name = "dark", foreground = "#ebdbb2", background = "#282828" }
