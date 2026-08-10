# Wombat

Wombat is an experimental Lua-powered dotfiles compiler. It evaluates Lua
configuration into an explicit, inspectable manifest before any target mutation
takes place.

The project is in early vertical-slice development. `build` evaluates
configuration and materialises a deterministic, self-contained build product
without changing a target. `add` is an explicit authoring command that copies
one existing home file into Wombat source state; deployment is not implemented
yet.

## Repository shape

Wombat keeps executable policy physically separate from deployable files:

```text
wombat.lua
modules/
  dot_config/
    starship.lua
dot_config/
  starship.toml
```

An anchored module can declare the common path without a destination:

```lua
local w = require("wombat")

w.install("starship.toml")
```

The manifest resolves that declaration to `~/.config/starship.toml`, including
its typed target anchor, inference provenance, final content digest, size, and
executable intent. Files beneath `dot_config/` are always opaque artifact
bytes, including Neovim and other Lua files.

## Building

Wombat does not discover a repository from the current directory. Select it
explicitly:

```sh
cargo run -- --source /path/to/dotfiles build
```

or configure it in `$XDG_CONFIG_HOME/wombat/config.toml` (falling back to
`~/.config/wombat/config.toml`):

```toml
format_version = 1
repository = "~/dotfiles"
```

Without either, the source defaults to `~/.local/share/wombat/`. Builds default
to `<source>/build/`; `-B work` selects `<source>/work`, while an absolute `-B`
path is used unchanged.

```text
build/
├── manifest.json
├── tree/
│   ├── home/
│   └── config/
└── .wombat/
```

`manifest.json` and `tree/` are the relocatable functional product. `.wombat/`
holds locking, staging, ownership, and recovery state. Rebuilding is staged and
verified before publication; it reports whether the product was created,
updated, unchanged, or repaired.

## Adding an existing file

An initialized repository selects a normal generated module once:

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

Then an existing regular file can be imported with:

```sh
cargo run -- --source /path/to/dotfiles add ~/.config/starship.toml
```

This copies its bytes to `dot_config/starship.toml` and adds an ordinary,
inspectable `w.install("dot_config/starship.toml")` declaration to the generated
region. Existing source files with different contents are refused; re-add and
force workflows are deliberately deferred.

## Development

Wombat requires a current stable Rust toolchain. Lua 5.5.0 is built into the
binary, so a separate Lua installation is not required.

```sh
cargo build
cargo test --all-targets
cargo run -- --source tests/fixtures/walking build -B "$PWD/target/walking-build"
cargo run -- --source tests/fixtures/paths build -B "$PWD/target/paths-build"
```

Run the complete local verification set with:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

VS Code users can open the committed workspace:

```sh
code wombat.code-workspace
```

The Lua API and manifest are intentionally provisional while the core model is
being proven against real dotfiles.
