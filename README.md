# Wombat

Wombat is an experimental Lua-powered dotfiles compiler. It evaluates Lua
configuration into an explicit, inspectable manifest before any target mutation
takes place.

The project is in its first implementation slice. The current `build` command
evaluates configuration and prints a deterministic JSON manifest; it does not
copy or modify files.

## Development

Wombat requires a current stable Rust toolchain. Lua 5.5.0 is built into the
binary, so a separate Lua installation is not required.

```sh
cargo build
cargo test --all-targets
cargo run -- build tests/fixtures/walking
```

Run the complete local verification set with:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

VS Code users can open the committed workspace:

```sh
code wombat.code-workspace
```

The Lua API and manifest are intentionally provisional while the core model is
being proven against real dotfiles.
