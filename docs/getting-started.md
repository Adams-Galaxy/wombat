# Getting started

This walks you from an empty directory to a file Wombat manages on your machine.
It should take a few minutes.

You need Rust 1.89 or newer. Lua is built into the binary, so you don't need to
install it separately.

## Install

```sh
cargo install --git https://github.com/Adams-Galaxy/wombat \
  --branch main --locked --force --root "$HOME/.local" wombat
```

Wombat installs in `~/.local/bin`. Keep that `--root "$HOME/.local"` option
when updating so Cargo does not create a second executable under `~/.cargo/bin`.

Or, from a clone, `cargo build --release` and use `./target/release/wombat`.

There's also an installer that can fetch Wombat and run a setup in one go, which
is covered under [fresh machines](#a-fresh-machine) below.

## Create a repository

Wombat never guesses which repository you mean from your current directory —
not even if you're standing in it. You either pass `--source` every time, or set
a default once. Let's create one:

```sh
wombat init ./dotfiles
```

That scaffolds the smallest repository that works:

```text
dotfiles/
├── wombat.lua        selects modules
├── wombat.toml       repository settings
├── modules/auto.lua  a generated region for `wombat add`
├── src/              the files you want deployed
└── .gitignore        ignores the default build/ workspace
```

`init` won't overwrite anything you've written by hand, and running it twice is
fine.

Every command below passes `--source ./dotfiles`. To stop typing it, record a
default once and drop the flag:

```sh
wombat config set-source ./dotfiles
wombat config show            # confirms what's resolved, and why
```

## Add a file

Put something in `src/`. The `dot_` prefix becomes a leading `.` in the
deployed path:

```sh
mkdir -p dotfiles/src/dot_config
printf 'format = "wombat"\n' > dotfiles/src/dot_config/starship.toml
```

Then declare it. Open `dotfiles/modules/auto.lua` and make it read:

```lua
local w = require("wombat")

-- wombat:add begin
w.install(".config/starship.toml")
-- wombat:add end
```

`src/dot_config/starship.toml` will become `.config/starship.toml` under your
home directory. If you'd rather not hand-write that, `wombat add` does the same
thing for a file that already exists — see
[add existing files](how-to/add-existing-files.md).

## Look before you leap

This is the part that makes Wombat different from a symlink manager. Build the
plan first, and read it:

```sh
wombat --source ./dotfiles plan construct
wombat --source ./dotfiles plan inspect
```

`plan construct` runs your Lua once and freezes the result — every artifact,
requirement, task, and script it intends to execute. Nothing has touched your
home directory yet.

Now produce the actual files, still without deploying:

```sh
wombat --source ./dotfiles plan materialise
wombat --source ./dotfiles inspect artifacts
wombat --source ./dotfiles explain ~/.config/starship.toml
```

You now have a complete product in `dotfiles/build/`: a `manifest.json` describing
everything, and a `tree/` holding the exact bytes that would be deployed.
`explain` traces a single artifact back to the declaration that produced it.

## See what would change

```sh
wombat --source ./dotfiles diff
```

`diff` is read-only. It compares three things — what Wombat deployed last time,
what's on disk now, and the product you just built — and tells you what it would
do.

## Deploy

```sh
wombat --source ./dotfiles apply
```

`apply` does construct, materialise, and deploy in one step, so it's what you'll
normally run day to day. It creates and updates files it owns, removes ones it
used to own, and leaves everything else alone. If it finds a file it doesn't
manage sitting where an artifact should go, it asks. In a script, where nobody
can answer, it fails instead of guessing — pass `--conflict` to decide in
advance.

Change the source file and run `diff` again to see the update, then `apply` to
take it.

## A fresh machine

On a machine with nothing on it, one command clones a repository, builds it,
installs any packages it declares, and deploys the result:

```sh
curl -fsSL https://raw.githubusercontent.com/Adams-Galaxy/wombat/main/install.sh \
  | sh -s -- setup owner/dotfiles
```

`setup` clones or reuses a matching checkout, freezes one plan, shows you the
package work before doing it, then deploys that exact build. It never pulls over
your changes, switches branches, or cleans an existing checkout.

## Where to go next

- [How Wombat works](concepts/how-wombat-works.md) explains the three stages you
  just used.
- [Render templates](how-to/render-templates.md) when a file needs values in it.
- [Declare requirements](how-to/declare-requirements.md) when your config needs
  a program installed.
- [`examples/dotfiles`](../examples/dotfiles) is a full worked repository with
  modules, templates, tasks, scripts, and packages.
