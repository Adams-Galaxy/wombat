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

There's also `w.need.package()` and `w.prefer.package()` for when you want a
named package rather than a command on `PATH`.

Notice there's no `brew install` anywhere. Modules declare products; the root
decides how they're obtained.

## Choose providers

Only root configuration selects providers, in priority order:

```lua
-- wombat.lua
w.providers({ "brew" })
```

Wombat ships Homebrew for macOS and Apt for Debian-family Linux. A provider that
doesn't suit the target refuses it — Homebrew won't resolve for a Linux target —
so selecting several and letting the target decide is normal. Custom providers
are ordinary tracked Lua.

## Deadlines

A requirement is satisfied before the rung it declares, defaulting to
`materialise.before`. Say so when something is only needed later:

```lua
w.need.command("python3", { when = w.rungs.materialise.tasks })
```

That matters because it decides how early a build fails. A tool only needed by a
task shouldn't block the whole build before anything has run.

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
