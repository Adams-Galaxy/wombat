# Declare requirements

Say what your configuration needs, not how to install it. Wombat resolves
requirements through providers and reconciles them at the point they're actually
needed.

## Declare what you need

```lua
local w = require("wombat")

w.need.command("git")
w.need.command("python3", { minimum = "3.12" })
w.prefer.command("rg", { accept = { "grep" } })
```

`need` is required: if it can't be satisfied, the build fails. `prefer` is
optional: an unsatisfiable one is reported and the build continues. `accept`
lists alternatives that also count as satisfied.

There's also `w.need.package()` and `w.prefer.package()` for when there's
nothing on `PATH` to check — a meta-package, a font, a certificate:

```lua
w.need.package("build-essential")
```

Notice there's no `brew install` anywhere. Modules declare products; the root
decides how they're obtained.

Like `command`, an unpinned `package` tries every configured provider in
priority order. Since package names and options aren't portable between
ecosystems, name `provider` explicitly once more than one is configured and the
package exists on only one of them:

```lua
w.need.package("build-essential", { provider = "apt" })
```

## Choose providers

Only root configuration selects providers, in priority order:

```lua
-- wombat.lua
w.providers({ "brew" })
```

Wombat ships Homebrew for macOS, Apt for Debian-family Linux, and Git for
anything installed by cloning it — a tmux/vim plugin, a version manager. A
provider that doesn't suit the target refuses it — Homebrew won't resolve for a
Linux target — so selecting several and letting the target decide is normal.
Custom providers are ordinary tracked Lua.

Git packages need an absolute destination, since there's no implicit install
location the way there is for a package manager:

```lua
local p = w.paths

w.providers({ "git" })
w.need.package("tpm", {
    with = { repository = "https://github.com/tmux-plugins/tpm.git", to = p.home .. "/.tmux/plugins/tpm" },
})
```

See [the Lua reference](../reference/lua.md#git) for `with.ref` pinning and
what happens when the destination already exists.

## Deadlines

A requirement is satisfied before the rung it declares, defaulting to
`materialise.before`. Say so when something is only needed later:

```lua
w.need.command("python3", { when = "materialise.tasks" })
```

That matters because it decides how early a build fails. A tool only needed by a
task shouldn't block the whole build before anything has run.

Name the rung as a string. The typed handle `w.rungs.materialise.tasks` works
too, but a string is checked against the ladder, so a typo is reported rather
than resolving to something else. Handles are for building a ladder, not for
pointing at one — see [run tasks and scripts](run-tasks-and-scripts.md).

## See it before it happens

```sh
wombat check
```

`check` is read-only. It reports each requirement as satisfied, missing,
outdated, unavailable, or error, and exits `0`, `1`, or `2` accordingly — usable
from a script.

To see the plan without executing it:

```sh
wombat plan construct
wombat plan inspect requirements
wombat plan inspect providers
```

## Let it install things

`build`, `apply`, and `setup` reconcile provider work at the declared deadlines.
Wombat shows you the consolidated plan — including shared preparation like a
single Apt index refresh — and asks once:

```sh
wombat build          # prompts
wombat build --yes    # confirms in advance, for scripts
```

Shared preparation happens once, not once per package. Failures stop
sequentially, report what was done and what remains, and rely on rerunning being
idempotent rather than claiming package-manager rollback.

Nothing is installed until every decision, including deployment conflicts, has
been made. Declining leaves the machine untouched.

## Add a third-party Apt source

Declare a named source once in root provider policy, then reference it from the
packages that need it. Unreferenced sources are inert. This is the managed form
of Yazi's published Apt setup:

```lua
w.providers({
    {
        name = "apt",
        with = {
            sources = {
                yazi = {
                    uri = "https://yazi-rs.github.io/builds/",
                    suite = "stable",
                    components = { "main" },
                    key = {
                        url = "https://yazi-rs.github.io/builds/yazi-keyring.gpg",
                        format = "gpg",
                    },
                },
            },
        },
    },
})

w.need.package("yazi", {
    provider = "apt",
    publishes = { commands = { "yazi" } },
    with = { source = "yazi" },
    when = "deploy.before",
})
```

Wombat checks the key and source file as a prerequisite even if `yazi` is
already installed. It writes Deb822 under `/etc/apt/sources.list.d/`, keeps
keys under `/etc/apt/keyrings/`, refreshes the index once when needed, and only
then simulates and installs the package. These locations follow
[Apt's current source-file guidance](https://manpages.debian.org/testing/apt/sources.list.5.en.html);
[Yazi documents the equivalent one-line setup](https://yazi-rs.github.io/docs/installation/).

Existing differing files that Wombat does not own are a hard preflight conflict.
Use `replace = true` only when you intend Wombat to adopt its fixed path. An
unpinned key must use HTTPS; supply `key.sha256` to permit HTTP and to pin the
downloaded bytes. Removing the source from configuration does not prune files
from `/etc`.

## Building for another machine

Package reconciliation only makes sense when you're building for the machine
you're on. To construct a product for a different target, say so:

```sh
wombat build --compile-only -- --target linux/x86_64
```

That constructs artifacts and records which requirement gates were skipped, so
the product is honest about not having been reconciled. Scripts scoped to the
host additionally need `--allow-host-scripts`.

## Reading it back

```sh
wombat inspect requirements
wombat inspect providers
```

The manifest records each requirement, the candidates considered, which provider
bound it, and the resolution attempted — so a product built last month can still
tell you why it chose what it chose.
