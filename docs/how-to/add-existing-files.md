# Add existing files

You already have dotfiles on your machine. `wombat add` copies one into your
repository and declares it, so you don't have to do both by hand.

## Once, per repository

`add` writes declarations into a generated region. `wombat init` sets this up,
but if your repository predates it, add the module and select it:

```lua
-- wombat.lua
local w = require("wombat")
w.use("auto")
```

```lua
-- modules/auto.lua
local w = require("wombat")

-- wombat:add begin
-- wombat:add end
```

Wombat only writes between those markers, so the rest of the file is yours.

## Add a file

```sh
wombat --source ~/dotfiles add ~/.config/starship.toml
```

That copies the bytes to `src/dot_config/starship.toml` and adds
`w.install(".config/starship.toml")` to the generated region. Nothing about your
home directory changes — `add` only writes to repository source.

## Add a directory

```sh
wombat --source ~/dotfiles add ~/.config/nvim
```

Directories are copied recursively, hidden and ordinary files alike, preserving
executable bits, and produce a single directory declaration rather than one line
per file.

Every leaf is checked before anything is written. Symlinks, special files, empty
trees, ownership conflicts, partial coverage, and existing source files with
different contents are all refused — and refused before any copying starts, so a
rejected `add` leaves your repository untouched.

## When a directory already covers it

If an existing directory declaration already covers the file you're adding, and
maps it to the same target, `add` copies just the file and tells you which module
owns it. No new declaration is written, because one already applies.

That works without `modules/auto.lua` at all. The generated module is the
fallback for files nothing covers yet.

## Adding from somewhere other than home

```sh
wombat --source ~/dotfiles add --target-root /etc/skel /etc/skel/.bashrc
```

The target must be a strict descendant of the root, which is what determines the
target-relative path it deploys back to.

## Things it deliberately won't do

Re-adding a file whose source already exists with different contents fails.
There's no `--force`: if you want to take the machine's version, delete the
source file and add it again, or edit the source directly. Making that a flag
would make it easy to overwrite work you meant to keep.

## After adding

Check what it did before deploying anything:

```sh
wombat build
wombat explain ~/.config/starship.toml
wombat diff
```

`diff` should show the file as an adoption — the target already matches the
product, so deployment has nothing to write.
