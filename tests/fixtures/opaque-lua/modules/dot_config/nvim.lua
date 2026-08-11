local w = require("wombat")
w.module.from(".config")
local helper = require("shared")

assert(helper == "control helper")
w.install("nvim/init.lua")
w.install("nvim/lua/plugins/example.lua")
