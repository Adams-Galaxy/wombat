# Contributing to Wombat

Wombat is pre-release software under active design. Discuss substantial Lua
surface, persisted-format, execution-policy, or architectural changes before
investing in an implementation. Small correctness, diagnostics, test, and
documentation improvements are welcome directly.

## Development

Wombat supports Rust 1.89 and newer stable toolchains. Lua 5.5 is vendored and
does not need to be installed separately.

Before submitting a change, run:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
```

Keep changes behaviorally focused. Add real Lua fixtures for language behavior,
exercise target mutation only against temporary roots, and preserve precise
diagnostics for important failures. Do not add compatibility machinery for
unreleased formats unless the maintainers have explicitly decided to support
it.

By contributing, you agree that your contribution is licensed under the same
MIT OR Apache-2.0 terms as Wombat.
