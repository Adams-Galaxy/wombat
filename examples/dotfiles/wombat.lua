local w = require("wombat")

local input = w.inputs({
    name = w.input.string({ default = "Wombat User", help = "Git author name" }),
    email = w.input.string({ default = "wombat@example.invalid", help = "Git author email" }),
    theme = w.input.choice({ values = { "dark", "light" }, default = "dark" }),
})

if w.target.os.name == "macos" then
    w.providers({ "brew" })
else
    w.providers({ { name = "apt", with = { update = true } } })
end

w.need.command("git")
local search = w.prefer.command("rg", {
    accept = { { name = "grep" } },
})

w.use("theme", { name = input.theme })
w.use("shell", { search = search.name })
w.use("git", { name = input.name, email = input.email })
w.use("editor")
w.use("tools")
