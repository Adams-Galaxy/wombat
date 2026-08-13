# Contributing to Wombat

Wombat is pre-release software under active design, and mostly a personal
project at this point. Discuss substantial Lua surface, persisted-format,
execution-policy, or architectural changes before investing in an
implementation. Small correctness, diagnostics, test, and documentation
improvements are welcome directly.

## Development

Wombat supports Rust 1.89 and newer stable toolchains. Lua 5.5 is vendored and
doesn't need installing separately.

```sh
cargo build
cargo test --all-targets
```

Before submitting a change, run the full local set:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
```

If you touched dependencies or `install.sh`, also run `cargo deny check` and
`shellcheck install.sh`.

## Validating properly

A suite that has only run in your checkout, on your machine, hasn't really been
validated — those are the two variables most likely to be wrong, and holding them
constant hides whole classes of bug. Wombat has been bitten by both: golden
fixtures that only matched in the directory that generated them, and tests that
inherited the host and so only passed on macOS.

Before claiming a change is green:

- run the suite from a **second checkout path**, which costs nothing and catches
  location coupling;
- run it on **Linux** as well as macOS, in a container or VM.

CI does both, so pushing is a reasonable way to get that coverage. Tests that
depend on the host should describe it with a fixture host rather than inheriting
the machine.

## Documentation

User-facing documentation lives in [`docs/`](docs/README.md) as plain Markdown.
It's deliberately small and not published anywhere yet. If you change a CLI flag
or a Lua function, update `docs/reference/cli.md` or `docs/reference/lua.md` in
the same change.

There's one gate for this. `documented_surface_has_not_drifted` records the CLI
and Lua surface and fails when it moves:

```sh
WOMBAT_BLESS_SURFACE=1 cargo test --bins documented_surface
```

It only notices that the surface changed — it can't tell whether the prose is
now correct. That part is still on you.

Rust internals are documented inline; `cargo doc --open` renders them.

## House style

Documentation is friendly and direct: second person, plain words, commands first
and explanation after. British spelling (`materialise`), and consistent
vocabulary — construct, materialise, deploy, plan, product, target, artifact,
module.

## Design notes

The design notebook — plans, proposals, investigations, and decision records —
lives in an ignored `notes/` directory and isn't part of the repository. It
records superseded reasoning on purpose, so it isn't a description of current
behaviour and public documentation never links into it. If you need the "why"
behind something and can't find it in `docs/concepts/`, ask.

## Scope

Keep changes behaviourally focused. Add real Lua fixtures for language
behaviour, exercise target mutation only against temporary roots, and preserve
precise diagnostics for important failures. Don't add compatibility machinery for
unreleased formats unless we've explicitly decided to support it — when a format
changes, Wombat bumps the version and asks for a rebuild.

By contributing, you agree that your contribution is licensed under the same
MIT OR Apache-2.0 terms as Wombat.
