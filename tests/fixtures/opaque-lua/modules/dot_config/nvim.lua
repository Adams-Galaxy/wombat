local w = require("wombat")
local helper = require("shared")

assert(helper == "control helper")
w.install("nvim/init.lua")
w.install("nvim/lua/plugins/example.lua")
