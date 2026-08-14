# CLI reference

Run `wombat <command> --help` for the authoritative flags; this page adds the
behaviour that help text can't carry.

## Global options

Available on every command.

| Option | Meaning |
| --- | --- |
| `-S`, `--source <PATH>` | Source repository. Defaults to the configured source, then `~/.local/share/wombat` |
| `--color <auto\|always\|never>` | Colour policy for human output. Default `auto` |
| `--trace` | Include filtered user frames and underlying diagnostic evidence |
| `--log-level <debug\|info\|notice\|warn\|error>` | Override the repository's minimum log level |
| `-v` / `-q` | One more / one fewer level of operational detail, repeatable |
| `-h`, `--help` / `-V`, `--version` | Help and version |

Wombat never infers the repository from your working directory. `--color auto`
honours `NO_COLOR` and keeps redirected output plain, and colour is never the
only signal.

## Build directories

Commands that take `-B`/`--build-dir` default to `build`. A relative path is
resolved against the source repository, so `-B build/server` lives inside it. An
absolute path is used unchanged, which is how a product can be inspected or
deployed without its repository.

## Repository inputs

`build`, `apply`, `setup`, `check`, and `plan construct` accept repository-defined
inputs after `--`. Wombat's own options and your repository's are separate
namespaces:

```sh
wombat build --help        # Wombat's options
wombat build -- --help     # your repository's inputs
wombat build -B build/server -- --target linux/x86_64 --theme kanagawa
```

## Commands

### `build`

Construct and materialise a product. Does not deploy.

`--compile-only` constructs for a target that isn't this machine, skipping
provider reconciliation. `--clean` reconstructs from scratch. `--yes` confirms
package work non-interactively. `--rerun-scripts` forces scheduled scripts.
`--allow-host-scripts` authorises compile-only scripts scoped to the host.

`--skip-requirements` and `--skip-scripts` trade safety for speed on the
common edit-and-rebuild loop, where a fresh cached product still needs to
avoid the cost of a package check or a script run. `--skip-requirements`
skips checking packages and commands against the host; unlike
`--compile-only`, a cached product from a prior full build is still reused.
`--skip-scripts` skips every `w.script` entry; `w.build.task` entries still
run, since they produce artifacts the build depends on. Both default to off
— nothing is skipped unless asked for.

### `plan construct | materialise | inspect | deploy`

The three stages, separately, plus inspection of a stored plan.

`construct` evaluates Lua once and persists an executable plan. `materialise`
executes that exact plan without evaluating Lua again, and takes the same
`--compile-only`, `--clean`, `--yes`, `--skip-requirements`,
`--skip-scripts`, `--rerun-scripts`, and `--allow-host-scripts` options as
`build`.

`deploy` deploys a completed product without reconstructing it. Alongside
`--target-root`, `--conflict`, `--yes`, `--skip-requirements`,
`--skip-scripts`, `--rerun-scripts`, and `--allow-host-scripts`, it takes two
acknowledgements that `apply` never needs because `apply` builds the product
itself:

- `--allow-plan-mismatch` deploys a materialised product even though a newer
  plan has since been constructed. Wombat warns because the product is no longer
  the newest intent.
- `--allow-compile-only` deploys a product built with `--compile-only`, whose
  requirement gates were skipped rather than reconciled.

`inspect` reads a stored plan; sections are `overview`, `providers`,
`requirements`, `ladder`, `scripts`, `tasks`, `artifacts`, `sources`,
`observations`.

### `apply`

Construct, materialise, then deploy. The usual day-to-day command.

`--target-root` chooses the deployment root, defaulting to your home.
`--conflict <ask|fail|skip|overwrite>` decides conflicts in advance; see
[ownership and deployment](../concepts/ownership-and-deployment.md). Also takes
`--clean`, `--yes`, `--skip-requirements`, `--skip-scripts`,
`--rerun-scripts`, and `--allow-host-scripts` — see `build` above for what the
skip flags do.

### `setup <REPOSITORY>`

Acquire a repository, then run the same guarded apply workflow. Built for a
machine with nothing on it.

`Adams-Galaxy` expands to that owner's `dotfiles` on GitHub; `owner/repository`
expands to GitHub HTTPS; `--ssh` changes only that shorthand expansion. Explicit
HTTPS, SSH/SCP, `git+`, `file://`, and local paths are used as given.

Setup reuses an existing checkout only when its origin matches. It never pulls,
switches branches, changes remotes, or cleans your working tree.

### `check`

Read-only. Reports whether this environment satisfies a completed product:
satisfied, missing, outdated, unavailable, or error. Exits `0` when satisfied,
`1` when not, `2` on an operational failure. `--compile-only` disables provider
gates.

### `diff`

Read-only comparison of a product against a target root. `--patch` includes
complete patch bodies; by default creates, adoptions, and removals are compact
while modifications and conflicts show their focused patch.

### `inspect [SECTION]`, `explain <ARTIFACT>`, `compare [PRODUCTS]...`

Read a verified product without evaluating Lua.

`inspect` sections are `overview`, `inputs`, `target`, `modules`,
`dependencies`, `providers`, `requirements`, `ladder`, `scripts`, `tasks`,
`artifacts`, `sources`, `observations`, `timeline`.

`timeline` is the build log: every rung and action the execution journal
recorded, slowest first, with how long each took. It's the first place to look
when a build or apply that used to be fast suddenly isn't — a regression shows
up as "this got slower" instead of requiring an external profiler.

`explain` takes an artifact target, logical path, or anchored source path and
traces it to its owner, declaration, source origin, production mode, target
inference, and frozen template context. A source excerpt appears only when your
repository file still matches the digest the product recorded.

`compare` with one path compares `build` to it; with two, compares them
directly. It reports semantic changes and hides what didn't move.

### `config show` · `config set-source [PATH]`

Wombat's own configuration, rather than a repository's.

`config show` prints the resolved source repository, which of the three
resolution routes chose it — an explicit `--source`, the configured
`repository`, or the built-in default — and the config file path. It warns when
the resolved directory has no `wombat.lua` yet. Since Wombat never infers the
repository from your working directory, this is usually the fastest answer to
"why is it building that?".

`config set-source` records a default so `--source` isn't needed. `PATH`
defaults to the current directory:

```sh
cd ~/dotfiles
wombat config set-source
```

It rewrites only the `repository` line, so comments, `[runners]` entries, and
your formatting survive. If no config file exists, it writes a minimal one.

### `init [PATH]`

Create the smallest conventional repository: `wombat.lua`, `wombat.toml`,
`modules/auto.lua`, `src/`, and a `.gitignore` for the default build workspace.
It tolerates unrelated files, is idempotent for the exact scaffold, and refuses
to overwrite handwritten policy. It does not initialise Git, build, deploy, or
change your configured source.

### `add <TARGET>`

Copy one existing file or directory tree into source state and declare it. See
[add existing files](../how-to/add-existing-files.md).

`--target-root` sets the root the target path is derived from, defaulting to your
home. `add` changes repository source only; it never writes to the target.

### `completions <SHELL>`

Print a completion script for `bash`, `zsh`, `fish`, `elvish`, or `powershell`
to stdout. For zsh:

```sh
wombat completions zsh > "${fpath[1]}/_wombat"
```

Any directory on `$fpath` works; pick one your shell already scans, then start
a new shell. The script is generated from Wombat's own argument definitions,
so it never drifts from `--help`.

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Success. For `check`, the environment is satisfied |
| `1` | The command failed. For `check`, requirements are unsatisfied |
| `2` | Usage error, or an operational failure during `check` |
