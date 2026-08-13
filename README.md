# Wombat

Wombat is a Lua-powered dotfiles compiler and machine bring-up orchestrator for
macOS and Linux.

It evaluates your Lua configuration into an explicit, inspectable plan, executes
that exact plan to build a verified product, and then deploys it under guard. You
can read the whole plan before anything touches your machine.

```lua
-- modules/shell.lua
local w = require("wombat")
local theme = w.using("theme")

w.module.from(".config")
w.install("starship.toml", { with = { colors = theme.colors } })
```

```sh
wombat plan construct   # freeze what will happen
wombat plan inspect     # read it
wombat diff             # see what would change on disk
wombat apply            # do it
```

## Why

Most dotfile managers either symlink a directory and hope, or run a script and
hope harder. Wombat compiles instead: conventions become an explicit artifact you
can inspect, diff, and explain, and only then does anything get written.

- **Look before you leap.** Construction freezes every artifact, package,
  task, and script before one of them runs.
- **It owns only what it declares.** Files you never told it about are never
  touched, and edits you made by hand become explicit conflicts rather than
  casualties.
- **Nothing happens until every decision is made.** Decline a conflict and no
  package was installed and no file was written.
- **The product is portable and exact.** Same configuration, same build
  identity, any machine.

## Install

Requires Rust 1.89 or newer. Lua 5.5 is built into the binary.

```sh
cargo install --git https://github.com/Adams-Galaxy/wombat --branch main --locked wombat
```

On a fresh machine, one command can fetch Wombat, build your repository, install
what it declares, and deploy it:

```sh
curl -fsSL https://raw.githubusercontent.com/Adams-Galaxy/wombat/main/install.sh \
  | sh -s -- setup owner/dotfiles
```

## Getting started

```sh
wombat init ./dotfiles
cd dotfiles
# put a file in src/, declare it in modules/auto.lua
wombat apply
```

The full walkthrough is in [docs/getting-started.md](docs/getting-started.md).

## Documentation

Everything lives in [`docs/`](docs/README.md):

- [Getting started](docs/getting-started.md)
- Concepts — [how Wombat works](docs/concepts/how-wombat-works.md),
  [modules and sources](docs/concepts/modules-and-sources.md),
  [ownership and deployment](docs/concepts/ownership-and-deployment.md),
  [products and formats](docs/concepts/products-and-formats.md)
- Reference — [CLI](docs/reference/cli.md), [Lua API](docs/reference/lua.md),
  [configuration](docs/reference/configuration.md),
  [formats](docs/reference/formats.md)
- How-to — [add existing files](docs/how-to/add-existing-files.md),
  [render templates](docs/how-to/render-templates.md),
  [declare requirements](docs/how-to/declare-requirements.md),
  [run tasks and scripts](docs/how-to/run-tasks-and-scripts.md)

Worked examples: [`examples/minimal`](examples/minimal) and
[`examples/dotfiles`](examples/dotfiles).

## Status

Pre-1.0 and honest about it. The product works and is tested on macOS and Linux,
but the Lua API, the Rust library, and the persisted formats all still change
between releases. When a format moves, Wombat tells you to rebuild rather than
migrating quietly.

Not implemented, deliberately: whole-deployment rollback, remote deployment,
encrypted secrets, symlink artifacts, and Windows.

Repository Lua, tasks, scripts, and providers are trusted code that runs on your
machine. Read a Wombat repository before running it, like any other program.

## Development

```sh
cargo build
cargo test --all-targets
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the full check set and how the project
is organised.

## Licence

MIT or Apache-2.0, at your option. See [LICENSE-MIT](LICENSE-MIT) and
[LICENSE-APACHE](LICENSE-APACHE).
