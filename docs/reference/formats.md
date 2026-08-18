# Formats reference

Everything Wombat persists is versioned. When a format's shape or meaning
changes, the version is bumped and older files are rejected with a message
telling you to rebuild. There are no migrations — see
[products and formats](../concepts/products-and-formats.md) for why.

## Current versions

| Format | Version | Location |
| --- | --- | --- |
| Manifest | 21 | `<build>/manifest.json` |
| Build plan | 12 | `<build>/.wombat/plan/plan.json` |
| Execution journal | 4 | `<build>/.wombat/execution-journal.json`, and per target |
| Target state | 3 | `$XDG_STATE_HOME/wombat/targets/<target>/state.json` |
| Script state | 1 | `$XDG_STATE_HOME/wombat/scripts/materialise/<project>/…/state.json` |
| Workspace marker | 1 | `<build>/.wombat/workspace.json` |
| Derivation cache | 1 | `<build>/.wombat/cache/` |
| Repository config | 3 | `<repository>/wombat.toml` |
| User config | 2 | `$XDG_CONFIG_HOME/wombat/config.toml` |

Alongside these, `construction_version` is currently **3**. It gates product
compatibility and moves only when construction can produce different output for
unchanged configuration — unlike the release version, which changes whenever
Wombat is released and does not invalidate products.

## What each one holds

**Manifest** — the canonical product description, and the machine-readable
contract. Resolved inputs, target, consulted observations, source catalogue with
digests, modules and dependencies, template helper packs, ladder, providers,
requirements and their frozen per-binding elevation capability, checked
provider prerequisites, preparations, tasks, scripts, artifact policy and notices,
selections, artifacts, and the identities. Sealed after publication.

**Build plan** — the frozen intent from construction: everything Wombat is
permitted to do, before it does any of it. Materialisation consumes exactly this
and never re-evaluates Lua.

**Execution journal** — what actually happened when a ladder ran: per-rung and
per-action status, timing, and failure detail. Kept separate from the manifest so
products stay immutable; inspection combines the two. If no journal exists,
inspection says execution state is unavailable rather than inventing it.
Explicitly skipped requirement gates and script actions are recorded here with
their reason; skip controls never rewrite the sealed manifest.

**Target state** — Wombat's record of what it last deployed to a given target
root, which is what makes three-way reconciliation possible. Private, locked
during deployment.

**Script state** — scheduling state for `once` and `onchange` scripts, namespaced
per project. Survives `--clean`; `--rerun-scripts` overrides it without deleting
it.

**Workspace marker** — records which source a build directory belongs to, so
Wombat refuses to reuse a workspace that was built from somewhere else.

**Derivation cache** — verified template and task results, build-local only.
Never enters the product, so a cached build and a cold build produce identical
bytes.

## Identity

`plan_id` and `build_id` are SHA-256 digests over configuration content:
versions, source digests, inputs, resolved target, observations, modules,
dependencies, template helper packs, ladder, providers, requirements,
prerequisites, preparations, tasks, scripts, artifact policy and selections,
and artifacts.

They deliberately exclude where the repository sits on disk and which Wombat
release built it, so the same configuration yields the same identity anywhere.

`project_identity` is separate. It namespaces script state per checkout — or per
declared `project` — and takes no part in identity.

## When a version doesn't move

Adding an optional key to a config format doesn't bump its version, because files
that were valid stay valid. The version moves when previously valid data would
now be read differently, or when the shape genuinely changes.
