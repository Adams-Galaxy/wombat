# Modules and sources

Wombat keeps two things physically separate: the code that decides what you want,
and the files you actually want deployed.

```text
wombat.lua          the root; selects modules
modules/            Lua that decides things
  editor.lua
  tools.lua
src/                files that get deployed
  dot_config/
    starship.toml
```

Everything under `modules/` is executable policy. Everything under `src/` is
data — including Lua files, so your Neovim config stays an opaque artifact rather
than something Wombat tries to evaluate.

## Selecting modules

`w.use()` selects a module; `w.using()` consumes what another module exported and
records the dependency.

```lua
-- wombat.lua
local w = require("wombat")
w.use("theme", { name = "kanagawa" })
w.use("editor")
```

```lua
-- modules/editor.lua
local w = require("wombat")
local theme = w.using("theme")   -- editor now depends on theme
```

Modules are singletons. Selecting one twice with different configuration is an
error rather than a race, and cycles are refused with the path that formed them.
Module files are found by filename stem anywhere under `modules/`, so the
directory layout is yours to organise — it carries no meaning.

## From source path to target path

A module states where its sources live and where they land, then declares files
by their common name:

```lua
local w = require("wombat")

w.module.from(".config")
w.install("starship.toml")
```

That resolves `src/dot_config/starship.toml` to `.config/starship.toml` under the
deployment root.

The naming grammar in `src/` is generic — no target directory is special:

| Marker | Meaning |
| --- | --- |
| `dot_` | the component starts with `.` once deployed |
| `unalloc_` | this component exists in source only; it doesn't affect the target path |
| `@` | shorthand for `unalloc_` |
| `literal_` | escape a name that would otherwise look like a marker |

So `src/dot_config/nvim/init.lua` deploys to `.config/nvim/init.lua`, and a
directory named `@drafts` groups sources without appearing in the target.

Entries whose real name starts with a dot are invisible to selection unless you
ask for them with `w.hidden()`. That keeps `.DS_Store` and friends out by
default without a blocklist.

## Selecting more than one file

`w.install()` takes an exact name, a directory, or a glob:

```lua
w.install("nvim")                            -- every regular file beneath it
w.install("*.toml", { exclude = { "draft.toml" } })
w.install("themes/**", { allow_empty = true })
```

Directories expand recursively into their regular file leaves, in deterministic
order. Symlinks and special files are refused rather than silently skipped. Globs
are deterministic and support `exclude`; set-style selectors accept
`allow_empty` when matching nothing is legitimate.

However a file was selected, each leaf ends up an independently owned artifact
with its own digest, target path, and provenance. There's no such thing as a
directory artifact at deployment time.

## Ownership

A module owns exactly the artifacts it installs. Two modules installing to the
same target path is a conflict, reported at construction with both declaration
sites, because there's no sensible way to pick a winner.

This is what makes removal safe: when you delete a declaration, Wombat knows the
artifact used to be owned and can clean it up, without ever needing a list of
files it's allowed to delete.

## What ends up in the manifest

Each artifact records where it came from, how it was produced, what it resolved
to, its digest and size, and whether it's executable — plus the source line that
declared it. That's what `wombat explain` reads back to you:

```sh
wombat explain ~/.config/starship.toml
```

Because the resolution is recorded rather than recomputed, the answer is exact
even for a product you built on another machine.
