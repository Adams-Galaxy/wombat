# Minimal example

The smallest honest Wombat repository: exactly what `wombat init` scaffolds, plus
one file to deploy.

```text
wombat.lua                       selects the generated module
wombat.toml                      repository settings
modules/auto.lua                 one declaration, in the generated region
src/dot_config/starship.toml     the file itself
```

Build it without touching your home directory:

```sh
wombat --source examples/minimal build -B /tmp/minimal-build
wombat --source examples/minimal inspect -B /tmp/minimal-build artifacts
```

That produces one artifact, `.config/starship.toml`. To see deployment against a
scratch root rather than your real home:

```sh
wombat --source examples/minimal diff -B /tmp/minimal-build --target-root /tmp/minimal-home
```

Everything here except `src/dot_config/starship.toml` and the declaration in
`modules/auto.lua` is what you get from `wombat init`. For a realistic
configuration with modules, templates, tasks, scripts, and packages, see
[`../dotfiles`](../dotfiles).
