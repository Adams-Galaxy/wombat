# Products and formats

A *product* is what a build produces: a complete, verified, relocatable directory
describing exactly what should be deployed.

```text
build/
├── manifest.json     the typed description of everything
├── tree/             the exact bytes to deploy, including encoded external targets
└── .wombat/          plan, locks, caches, staging, recovery state
```

`manifest.json` and `tree/` are the product. `.wombat/` is working state and
never travels with it. Once published, the manifest and tree are sealed —
operational results like "this script ran" live in execution journals under
locked state, not written back into the product.

Root-relative payloads retain their target path below `tree/`. Explicit external
destinations use an encoded private namespace instead, so a destination such as
`/mnt/c/Users/Ada/.wezterm.lua` can never be interpreted as a path that escapes
the verified product tree.

That seal is what makes a product trustworthy. If it could be edited after
publication, inspecting one would tell you what it says about itself rather than
what it is.

## Build identity

Every product has a `build_id`: a SHA-256 over its complete configuration
content — sources and their digests, resolved inputs, the target, consulted host
observations, modules, dependencies, template helper packs, the ladder,
requirements, each binding's frozen elevation capability, provider
prerequisites, tasks, scripts, and artifacts.

Two properties follow, and both are deliberate.

**The same configuration gives the same identity anywhere.** Build the same
repository on two machines, or at two paths, and the `build_id` matches. That's
what makes reuse decisions, `wombat compare`, and rebuild-and-verify mean
something.

**Almost anything can change it.** Source digests include your Lua, so moving a
declaration between lines produces a new identity even when every deployed byte
is identical. That's the honest answer: the identity describes the configuration
that produced the product, not just its output.

Where the repository happens to sit on disk is *not* part of identity. Only
per-checkout operational state — script scheduling, mainly — is namespaced by
location.

## Structured source data

Configuration Lua can read repository-relative JSON, TOML, and strict YAML as
frozen data. Those reads use the same source catalogue as Lua modules and
artifacts, so changing a decoded data file changes construction identity.

Encoding travels in the other direction: it returns a string for
`w.generate()` and does not read or write the repository itself. JSON and YAML
accept every frozen root shape; TOML requires a map and cannot represent null.
YAML output is canonical structured data rather than a rewrite of the authored
document—comments, anchors, scalar styles, and layout are not preserved. Use a
template when presentation is part of the file you intend to maintain.

## Versioned formats, and no migrations

Everything Wombat persists carries a format version. When the shape or meaning
changes, the version is bumped and old files are rejected with a clear message:

```text
unsupported manifest format version <old>; expected <current>;
rebuild this product with the current Wombat
```

There is deliberately no migration code. Wombat is pre-1.0 and the formats are
still moving; supporting every historical shape would mean carrying compatibility
paths that are hard to test and easy to get subtly wrong. Rebuilding is cheap,
and a build is reproducible from your repository, so the old product isn't
something you needed to preserve.

If you see that message after upgrading, run your normal `build` or `apply`. It's
expected, not a fault.

## Upgrading Wombat doesn't invalidate your product

Wombat separates its *release version* from its *construction version*.

The release version is recorded in the manifest as provenance — useful to know,
but it isn't what compatibility is judged on. The construction version is, and it
only moves when construction can genuinely produce different output for unchanged
configuration.

So installing a new Wombat that changed a CLI message leaves your products valid.
A Wombat that changed how templates render, or how sources are selected, bumps
the construction version, and your products are rejected until rebuilt.

The trade is that this correctness now depends on that constant being bumped when
it should be, rather than falling out automatically from the release number. It
was worth it: the previous behaviour meant every release invalidated every
product on every machine.

## Reuse and caching

Because identity is content-derived, a repeated build can recognise that nothing
relevant changed and reuse the existing product rather than rebuilding it. You'll
see `unchanged` or `reused` instead of `created`.

Template rendering and task results are cached inside the build directory only.
Caches never enter the product, so a product built with a warm cache is
byte-identical to one built cold. `--clean` reconstructs from scratch while
preserving script scheduling state; `--rerun-scripts` forces scheduled scripts
without deleting their state.

Lua template helper sources are executable plan payloads, not product payloads.
Their exact transitive source closure is copied beneath `.wombat/plan`, verified
before materialisation, and recorded by digest in both plan and product. The
rendered product remains relocatable without carrying executable helper code.

For the exact current version numbers, see
[the formats reference](../reference/formats.md).
