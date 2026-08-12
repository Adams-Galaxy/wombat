local w = require("wombat")

local example = w.data.toml("example.toml")
local shell = w.exec({ "sh", "-c", 'printf %s "$WOMBAT_EXAMPLE_MODE"' }, {
    env = { WOMBAT_EXAMPLE_MODE = "canonical" },
}):check()
w.log.info("constructing canonical example", { mode = shell.stdout, owner = example.example.owner })

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

w.install(".wombat-example")

w.use("theme", { name = input.theme })
w.use("shell", { search = search.name })
w.use("git", { name = input.name, email = input.email, owner = example.example.owner })
w.use("editor")
w.use("tools")
