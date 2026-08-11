# Wombat

Wombat is an experimental Lua-powered dotfiles compiler. It evaluates Lua
configuration into an explicit, inspectable manifest before any target mutation
takes place.

The project is in early vertical-slice development. `init` creates the smallest
conventional source repository, and `build` evaluates
configuration and materialises a deterministic, self-contained build product
without changing a target. `setup` acquires a repository and carries one exact
product through requirement bootstrap and guarded deployment. `diff`, `apply`,
and `deploy` then inspect or
guardedly reconcile that exact product with a target home. `inspect`, `explain`,
and `compare` make exact completed products understandable without evaluating
Lua. `add` is an explicit authoring command that copies one existing home file
or regular-file directory tree into Wombat source state.

## Repository shape

Wombat keeps executable policy physically separate from deployable files:

```text
wombat.lua
modules/
  dot_config/
    starship.lua
  dot_local/
    tools.lua
dot_config/
  starship.toml
dot_local/
  bin/
    tool
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

`w.install()` also accepts a directory. Wombat recursively expands its regular
file leaves in deterministic order, including hidden files, while rejecting
symlinks and special entries:

```lua
-- modules/dot_config/nvim.lua
local w = require("wombat")

w.install("nvim") -- dot_config/nvim/** -> ~/.config/nvim/**
```

The three canonical artifact roots are `home/`, `dot_config/`, and
`dot_local/`; the last maps to `~/.local/`. Empty directories and directory
roots are not artifacts, and no source-tree ignore rules are applied.

## Templates

A terminal `.tmpl` suffix makes the ordinary installation path a template and
is removed from an inferred destination:

```lua
local w = require("wombat")
local theme = w.using("theme")

w.install("starship.toml.tmpl", {
    with = {
        colors = theme.colors,
        shell = "zsh",
    },
})
```

Lua assembles the complete context and Wombat freezes it at declaration. Rust
renders it during `build` with the versioned `handlebars-v1` contract: strict
missing values, no automatic escaping, deterministic maps, interpolation,
`if`/`unless`, `each`/`with`, comments, raw blocks, and whitespace control.
Comparisons and transformations belong in Lua; custom helpers, lookup, logging,
subexpressions, partials, and decorators are unavailable. Context is inspectable
manifest data and must not contain secrets.

Handlebars preserves ordinary template whitespace. Use explicit line structure
for generated configuration, and use `~` only when adjacent whitespace really
should disappear:

```handlebars
{{#if shell}}
shell = "{{shell}}"
{{/if}}

prefix={{~ value ~}}suffix
```

The first block emits complete lines when selected. The second deliberately
trims whitespace on both sides of `value`; careless trimming can join lines in
the generated file.

`w.install.file("literal.tmpl")` installs a `.tmpl` file literally, while
`w.install.template("input", { to = "~/.config/output", with = context })`
marks an unconventional name explicitly. Template directories, implicit
runtime context, includes, callbacks, and generated Lua artifacts are not yet
supported.

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

Create the minimal conventional repository at the selected source with:

```sh
wombat init

# Or scaffold an explicit source without changing Wombat's user configuration.
wombat init ./dotfiles
```

Initialization creates `wombat.lua`, a selected `modules/auto.lua` generated
region, and a new `.gitignore` for the default build workspace. It permits
unrelated files, is idempotent for the exact scaffold, and refuses to overwrite
handwritten policy or traverse reserved symlinks. It does not initialize Git,
build, deploy, or change the configured repository.

## Parameterised builds and context

Repositories may declare their own typed build inputs. They live after `--`, in
a namespace separate from Wombat's options:

```lua
local w = require("wombat")

local input = w.inputs({
    target = w.input.target(),
    theme = w.input.choice({
        values = { "gruvbox", "kanagawa" },
        default = "gruvbox",
        help = "Shared color theme",
    }),
    yazi = w.input.flag({ default = true }),
})

w.target(input.target)
w.use("theme", { name = input.theme })
if input.yazi then w.use("yazi") end
```

```sh
# Wombat-owned help and repository-owned help are distinct.
wombat build --help
wombat build -- --help

# Independent products can coexist beneath the repository.
wombat build -B build/local
wombat build -B build/server -- --target linux/x86_64 --theme kanagawa --no-yazi
```

The initial input kinds are `flag`, `choice`, `string`, `integer`, and `target`.
Flags support `--name` and `--no-name`; value inputs support both
`--name value` and `--name=value`. Invalid or unknown repository options fail
before module evaluation or materialisation.

The common path requires no target declaration. `w.target` begins as the
normalized local OS and architecture, while `w.host` exposes richer observed
facts such as OS and kernel versions, Linux distribution identity, hostname,
username, and home. Both are immutable Lua views:

```lua
local w = require("wombat")

if w.target.os.name == "macos"
    and w.target.os.version
    and w.target.os.version.major
    and w.target.os.version.major >= 15
then
    w.use("modern-macos")
end

if w.host.os.distribution and w.host.os.distribution.id == "fedora" then
    w.use("fedora")
end
```

Only facts actually consulted during evaluation enter manifest provenance.
Resolved inputs, the concrete target, consulted observations, and typed Lua
source provenance are stored in manifest v9 and participate in build identity.
Every Wombat-owned root, module, and repository `require()` load contributes its
repository-relative path and digest. Consequently a comment or declaration
line movement can produce a new exact build identity even when artifact bytes
are unchanged. Explicit template context is still the only route from context
values into rendered files.

## Inspecting completed products

Inspection opens and verifies exact products; it never evaluates repository
Lua:

```sh
wombat inspect
wombat inspect inputs
wombat inspect target
wombat inspect modules
wombat inspect dependencies
wombat inspect artifacts
wombat inspect sources

wombat explain ~/.config/starship.toml
wombat compare build/server
wombat compare build/linux build/macos
```

`inspect` provides an overview or focused manifest view. `explain` connects one
artifact to its owner, declaration trace, source origin, production mode,
target inference, frozen template context, and module relationships. A source
excerpt is shown only when the current repository file matches the product's
recorded digest. `compare` reports semantic source, input, target, module,
dependency, and artifact changes while hiding unchanged data.

Relative product paths remain relative to the selected source. Absolute
relocated products can be inspected and compared without their original
repository. The manifest remains the machine-readable product contract; these
commands intentionally provide human views rather than a second JSON schema.

## Requirements, bootstrap, and fresh-machine setup

Modules declare products rather than embedding package-manager commands:

```lua
local w = require("wombat")

w.need.command("git")
w.prefer.command("rg", { accept = { "grep" } })
```

Root policy selects ordered providers. Wombat currently includes Homebrew for
macOS and Apt for Debian-family Linux; custom providers are ordinary tracked
Lua. `check` is read-only, while `bootstrap` preflights, displays, confirms, and
post-checks explicit provider work:

```sh
wombat check
wombat bootstrap
```

For a fresh machine, `setup` safely clones or reuses a matching Git repository,
builds once, checks and optionally bootstraps requirements, then guardedly
deploys that exact build ID:

```sh
wombat setup Adams-Galaxy
wombat setup owner/repository --ssh
wombat setup https://github.com/owner/dotfiles.git -- --theme gruvbox
```

A single GitHub owner expands to `owner/dotfiles`; `owner/repository` also uses
GitHub HTTPS unless `--ssh` changes the shorthand. Explicit HTTPS, SSH/SCP,
`git+`, `file://`, and local paths are preserved. Setup never pulls, switches
branches, changes remotes, or cleans an existing checkout.

The development installer can obtain Wombat and forward directly into setup:

```sh
curl -fsSL https://raw.githubusercontent.com/Adams-Galaxy/wombat/main/install.sh \
  | sh -s -- setup Adams-Galaxy
```

Missing host build prerequisites require a separate interactive confirmation,
or leading `--install-prerequisites` in automation. Setup's `--yes` confirms
only the displayed bootstrap plan; deployment conflicts still use
`--conflict ask|fail|skip|overwrite`. The installer tracks `main` while Wombat
is pre-release; a stable `get.wombat.sh` endpoint and release binaries are
future distribution work.

## Diffing and deployment

The ordinary local workflow is:

```sh
wombat diff
wombat apply

# ergonomic build followed by apply of that exact build ID
wombat deploy
```

`deploy` accepts the same repository inputs as `build`, after `--`, and applies
the exact product it just built:

```sh
wombat deploy -B build/server -- --target linux/x86_64 --theme kanagawa
```

All three commands accept `-B/--build-dir` and `--target-home`. A relative build
directory remains relative to the configured Wombat source; an absolute build
product can be diffed or applied without its source repository.

Wombat compares the previous applied state, the current target, and the desired
verified build. Safe creates, updates, adoptions, and stale removals are
automatic. Unknown neighbouring files are untouched. Unmanaged collisions and
downstream changes prompt on an interactive terminal and fail safely in
non-interactive use unless a policy is explicit:

```sh
wombat apply --conflict fail
wombat apply --conflict skip
wombat apply --conflict overwrite
```

`skip` succeeds with an explicitly incomplete target state. There is no broad
`--force` behavior. Target files are replaced atomically per artifact; Wombat
does not claim whole-build rollback. Reconciliation state is private and lives
under `$XDG_STATE_HOME/wombat/targets/`, falling back to
`$HOME/.local/state/wombat/targets/`.

Deployment is currently supported on macOS and Linux. `TargetConfig` always
means the literal `<target-home>/.config`, independent of `XDG_CONFIG_HOME`.

An implicit deployment to the current home refuses a product whose target OS
does not match the observed host. Passing an explicit `--target-home` is the
deliberate escape route for testing or alternate roots; an architecture
mismatch is warned about but does not currently block deployment.

Diffs are adaptive: creates, adoptions, and removals are compact by default,
while modifications and conflicts include their focused patch. Use
`wombat diff --patch` to include complete applicable patch bodies.

Human-facing output uses semantic color when its stream is a terminal. Set
`--color always`, `--color never`, or use the default `--color auto`; auto also
honors `NO_COLOR` and keeps redirected output plain. Color is never the only
status signal.

## Adding existing files and directories

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

Then an existing regular file or directory can be imported with:

```sh
cargo run -- --source /path/to/dotfiles add ~/.config/starship.toml
cargo run -- --source /path/to/dotfiles add ~/.config/nvim
```

This copies its bytes to `dot_config/starship.toml` and adds an ordinary,
inspectable `w.install("dot_config/starship.toml")` declaration to the generated
region. Existing source files with different contents are refused; re-add and
force workflows are deliberately deferred.

If an already-selected directory declaration uniquely covers the new source
and maps it to the requested target, `add` copies only the file and reports the
owning module. That route works without `modules/auto.lua`; the generated module
remains the conservative fallback for files not covered by a directory.

A directory import recursively copies hidden and ordinary regular files,
preserves normalized executable intent, and writes one generated directory
`install()` declaration. Every leaf is preflighted before mutation. Symlinks,
special files, empty trees, conflicting ownership, partial coverage, and
different existing source state are refused without partial source mutation.

## Diagnostics

Wombat renders Lua and template failures as source-aware compiler diagnostics.
The concise default leads with the reason, user file and line, available source
excerpt, and relevant Wombat context such as module selection. Bundled Lua and C
frames are hidden.

```sh
wombat build
wombat --trace build
```

Global `--trace` adds up to eight filtered user frames and the underlying error
as fallback evidence. Tail-call frame loss is reported explicitly. Wombat does
not expose Lua's debug library or capture arbitrary locals and upvalues.

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
