# Wombat documentation

Wombat compiles Lua configuration into an explicit, inspectable plan, then
executes that exact plan. These pages explain how to use it and why it works the
way it does.

Wombat is pre-1.0. The Lua API, the Rust library, and the persisted formats all
change between releases, and these docs describe the current `main` rather than
a released version. They aren't published anywhere yet — they live here so you
can read them next to the code.

## Start here

- [Getting started](getting-started.md) — from nothing to a deployed dotfile.

## Concepts

Why Wombat is shaped the way it is. Read these when something surprises you.

- [How Wombat works](concepts/how-wombat-works.md) — construct, materialise,
  deploy.
- [Modules and sources](concepts/modules-and-sources.md) — how `src/` becomes
  target paths.
- [Ownership and deployment](concepts/ownership-and-deployment.md) — what
  Wombat will and won't touch.
- [Products and formats](concepts/products-and-formats.md) — build identity,
  versioned formats, and why upgrades ask you to rebuild.

## Reference

Exact contracts. Reach for these when you need the detail.

- [CLI](reference/cli.md)
- [Lua API](reference/lua.md)
- [Configuration](reference/configuration.md) — `wombat.toml`, user config,
  environment variables.
- [Formats](reference/formats.md) — persisted files and their versions.

## How-to

- [Add existing files](how-to/add-existing-files.md)
- [Render templates](how-to/render-templates.md)
- [Declare requirements](how-to/declare-requirements.md)
- [Run tasks and scripts](how-to/run-tasks-and-scripts.md)

## Examples

- [`examples/minimal`](../examples/minimal) — the smallest honest repository.
- [`examples/dotfiles`](../examples/dotfiles) — a realistic workstation
  configuration exercising most of the product.

Contributor-facing notes live in [`CONTRIBUTING.md`](../CONTRIBUTING.md), and
the Rust internals are documented through `cargo doc --open`.
