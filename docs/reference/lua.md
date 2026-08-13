# Lua API reference

Everything below comes from `require("wombat")`, conventionally bound to `w`.

All of it runs at **construction time** — while the plan is being frozen.
Nothing here executes during materialisation or deployment. Where an entry says
it "records" something, that means it goes into the plan for Rust to execute
later.

```lua
local w = require("wombat")
```

Unknown options are rejected everywhere rather than ignored, so a typo fails at
construction instead of quietly doing nothing.

Contents: [module selection](#module-selection) ·
[sources and artifacts](#sources-and-artifacts) ·
[generated content](#generated-content) · [inputs](#inputs) ·
[host and target](#host-and-target-context) ·
[requirements and providers](#requirements-and-providers) ·
[tasks and scripts](#tasks-and-scripts) · [ladders](#ladders) ·
[data, processes, logging](#data-processes-and-logging)

## Module selection

### `w.use(name, config?)`

Selects a module by filename stem, found anywhere under `modules/`.

- `name` — the stem, without `.lua`.
- `config` — optional table, frozen and readable inside the module via
  `w.module.config()`.

Records the module and a `use` dependency from the caller. Modules are
singletons: the first selection wins, and selecting the same module again with
*different* configuration fails.

Fails when the module isn't found, when two files share a stem, or when
selection forms a cycle — the error names the cycle path.

```lua
w.use("theme", { name = "kanagawa" })
```

### `w.using(name)`

Selects the module if it isn't already, evaluates it, and returns its frozen
export. Records a `using` dependency, which is what makes the dependency graph
in `wombat inspect dependencies` real rather than declared.

Returns whatever the module returned, deeply frozen — tables are read-only.

```lua
local theme = w.using("theme")
local accent = theme.colors.accent
```

### `w.module.config()`

Returns this module's frozen selection config, or an empty table when it was
selected without any.

```lua
local config = w.module.config()
local name = config.name or "gruvbox"
```

### `w.module.from(source, options?)`

Sets the module's source base, and by default its target base too.

- `source` — a path under `src/`, in the `src/` naming grammar, or a
  `w.hidden()` value.
- `options.to` — target base, when it differs from the source base.

Affects every subsequent `w.install()` in this module. Call it before
installing.

Module-only: calling it from root `wombat.lua` fails with "may only be called
from a selected module". Root configuration selects and composes; modules
declare.

```lua
w.module.from(".config")                       -- src/dot_config -> .config
w.module.from("@staging", { to = ".config" })  -- source only; target is .config
```

## Sources and artifacts

### `w.install(source, options?)`

Declares one or more artifacts. This is the call you'll use most.

- `source` — an exact name, a directory, a glob, or a `w.hidden()` value.
- `options.to` — explicit target path, overriding inference.
- `options.with` — template context. Supplying it makes the source a template.
- `options.exclude` — a glob string or array of globs to skip.
- `options.allow_empty` — boolean, default `false`. Permits a selector that
  matches nothing.

A trailing `.tmpl` on the source also makes it a template, and the suffix is
removed from the inferred target. Directories expand recursively into their
regular file leaves in deterministic order. Every leaf becomes an independently
owned artifact with its own digest and provenance.

Fails on symlinks and special files, on a selector that matches nothing unless
`allow_empty` is set, on an ambiguous relaxed lookup when both `name` and
`name.tmpl` exist, and when two modules claim the same target path.

```lua
w.install("starship.toml")
w.install("nvim")
w.install("*.toml", { exclude = { "draft.toml" } })
w.install("themes/**", { allow_empty = true })
w.install("starship.toml", { with = { colors = theme.colors } })
```

### `w.install.file(source, options?)`

Installs literally, never as a template — use it for a real `.tmpl` file you
want deployed unrendered. Passing `with` is an error.

```lua
w.install.file("literal.tmpl")
```

### `w.install.template(source, options?)`

Installs as a template even when the name doesn't end in `.tmpl`. Context
defaults to an empty table when `with` is omitted.

```lua
w.install.template("input", { to = ".config/output", with = context })
```

### `w.hidden(source)`

Selects a source entry whose real name begins with a dot. Dotted entries are
invisible to ordinary selection, which keeps `.DS_Store` and editor droppings
out without a blocklist.

```lua
w.install(w.hidden(".editorconfig"))
```

## Generated content

### `w.generate(name, options)`

Publishes bytes assembled in Lua as an ordinary artifact. Binary-safe.

- `name` — the declaration name.
- `options.content` — the bytes, as a string.
- `options.to` — target path.
- `options.executable` — boolean; sets executable intent.

No other options are accepted. The result has normal ownership, identity,
inspection, conflict, and deployment behaviour, and its content digest
participates in build identity.

```lua
w.generate("starship.toml", {
    content = rendered_bytes,
    to = ".config/starship.toml",
})
```

## Inputs

### `w.inputs(schema)`

Declares the repository's own command-line inputs and returns the resolved
values. Callers pass them after `--`, in a namespace separate from Wombat's own
options.

Call it once, in root configuration, before modules evaluate. Unknown or invalid
repository options fail before any module runs.

```lua
local input = w.inputs({
    target = w.input.target(),
    theme  = w.input.choice({ values = { "gruvbox", "kanagawa" }, default = "gruvbox" }),
    yazi   = w.input.flag({ default = true }),
})

w.target(input.target)
w.use("theme", { name = input.theme })
if input.yazi then w.use("yazi") end
```

### `w.input.flag|choice|string|integer|target(options?)`

The input kinds, used as schema values above.

- `options.default` — value used when the caller doesn't pass one.
- `options.help` — text shown by `wombat build -- --help`.
- `options.values` — permitted values. `choice` only.

Flags accept `--name` and `--no-name`. Value inputs accept both `--name value`
and `--name=value`. `target` parses a platform such as `linux/x86_64`.

Resolved inputs are recorded in the manifest and participate in build identity,
so two products built with different inputs are different products.

## Host and target context

### `w.host`

Immutable observed facts about the machine running construction: OS name and
version, kernel, Linux distribution identity, architecture, hostname, username,
and home.

### `w.target`

The resolved deployment target, starting as the normalised local OS and
architecture. Root configuration may replace it once, before anything reads it.

Only the facts you actually read are recorded as observations, and only those
participate in build identity — so reading `w.host.os.version` makes your
product depend on it, and not reading it doesn't.

```lua
if w.target.os.name == "macos" then
    w.use("macos-only")
end

if w.host.os.distribution and w.host.os.distribution.id == "fedora" then
    w.use("fedora")
end
```

### `w.paths.repository`

Absolute path to the source repository, as a string.

## Requirements and providers

### `w.need.command(name, options?)` · `w.need.package(name, options?)`

Declares something that must be available. A `need` that can't be satisfied
fails the build.

- `options.minimum` — minimum version string.
- `options.when` — rung deadline, as a canonical string. Default
  `materialise.before`.
- `options.accept` — alternatives that also satisfy it: a command string, a
  candidate table, or an array of either.
- `options.provider` (`package` only) — pin resolution to one configured
  provider. Omit it to try every configured provider in priority order, the
  same as `command` always does.

Name the rung as a string. `w.rungs.materialise.tasks` works too, but the string
is checked against the ladder — and an unknown rung is an error either way now,
so pick whichever reads better and stay consistent.

Fails when no configured provider can resolve the requirement, when providers
haven't been selected yet, and when `when` names a rung the ladder doesn't have.

```lua
w.need.command("git")
w.need.command("python3", { minimum = "3.12", when = "materialise.tasks" })
w.need.package("build-essential")
```

`package` is for something with nothing on `PATH` to check — a meta-package, a
font, a certificate. Since package names and options aren't portable between
ecosystems, pin `provider` explicitly once more than one provider is configured
and the package only exists on one of them:

```lua
w.providers({ "apt", "brew" })
w.need.package("build-essential", { provider = "apt" })
```

### `w.prefer.command(name, options?)` · `w.prefer.package(name, options?)`

The same options, but optional: an unsatisfiable `prefer` is reported rather
than fatal.

```lua
w.prefer.command("rg", { accept = { "grep" } })
```

### `w.providers(entries)`

Root-only — calling it from a module fails. Selects the ordered providers
permitted to satisfy requirements. Built-ins are `brew` (macOS), `apt`
(Debian-family Linux), and `git` (any target); custom providers are ordinary
tracked Lua.

A provider that doesn't suit the target refuses it, so listing several and
letting the target decide is normal. Requirements declared before this call
fail, because there'd be no policy to resolve them against.

```lua
w.providers({ "brew" })
```

### `git`

For a package that lives in a repository rather than a package manager — a
tmux/vim plugin, a version manager, anything installed by cloning it
somewhere. `with` takes:

- `with.repository` — a clone URL, required.
- `with.to` — an absolute destination path, required.
- `with.ref` — a branch, tag, or commit to pin. Omit it to track whatever the
  clone's default branch resolves to at clone time.

```lua
w.providers({ "git" })
w.need.package("tpm", {
    with = { repository = "https://github.com/tmux-plugins/tpm.git", to = w.host.home .. "/.tmux/plugins/tpm" },
})
```

An existing `with.to` is reused only when its `origin` remote already matches
`with.repository`; anything else there is left untouched and fails the build,
since it isn't `git`'s to overwrite. Checking is local — a pinned `ref` is
compared against what the last `reconcile` fetched, not against the network —
so a satisfied `git` package never makes a connection.

## Tasks and scripts

### `w.build.task(entrypoint, params?, options?)`

Declares a program under `tasks/` that produces artifacts, or acts as an
outputless build gate.

- `entrypoint` — path relative to `tasks/`.
- `params` — table passed to the task as JSON.
- `options.at` — rung. Default `materialise.tasks`. Tasks may only use rungs up
  to artifact construction.
- `options.cache` — boolean, or a table with an optional `revision` string to
  invalidate the cache deliberately. Default enabled.
- `options.logs` — `failure` (default), `always`, or `never`.
- `options.python_helper` — boolean, default `true`. Puts Wombat's Python helper
  on `PYTHONPATH`.
- `options.interpreter` — explicit runner, overriding inference.

Regular files left in the task's output directory become artifacts. Results are
cached against the task's inputs; caches stay inside the build directory and
never enter the product.

```lua
w.build.task("generate.py", { message = "Hello" })
w.build.task("validate.sh", {}, { cache = false })
```

### `w.script(entrypoint, params?, options?)`

Declares a program under `scripts/` as a stateful ladder action. Scripts cause
effects; they are not artifact factories.

- `entrypoint` — path relative to `scripts/`.
- `params` — table passed to the script as JSON.
- `options.at` — rung. Default `materialise.before`. Any leaf is allowed.
- `options.name` — ASCII letters, numbers, `-`, or `_`. Distinguishes two
  declarations of the same entrypoint.
- `options.schedule` — `always` (default), `once`, or `onchange`.
- `options.files` — array of globs whose digests drive `onchange`.
- `options.scope` — `target` (default) or `host`.
- `options.env` — string map of extra environment variables.
- `options.timeout` — positive integer seconds.
- `options.logs` — `failure` (default), `always`, or `never`.
- `options.revision` — opaque string; changing it forces `onchange` to fire.
- `options.python_helper` — boolean, default `true`.
- `options.interpreter` — explicit runner, overriding inference.

Worth knowing: when a script's runner is a bare command name and providers are
configured, Wombat automatically declares a matching `need.command` at the
script's rung. You don't have to declare the interpreter your script needs.

```lua
w.script("configure.py", { profile = "desktop" }, {
    at = "configure",
    schedule = "onchange",
    files = { "helpers/**" },
})
```

Both tasks and scripts infer Python, POSIX shell, Bash, embedded Lua 5.5, or a
direct executable entrypoint. A direct executable needs a shebang: macOS runs a
shebang-less text file under a shell, Linux returns `ENOEXEC`.

## Ladders

### `w.rungs`

The eight mandatory core events as typed handles:

```text
materialise.before   materialise.tasks   materialise.artifacts
materialise.publish  materialise.after
deploy.before        deploy.apply        deploy.after
```

Read-only. Accessing a name that doesn't exist raises `unknown rung handle`
rather than returning nil, because a nil `at` or `when` would otherwise mean
"unspecified" and silently move the action to its default rung.

### `w.rung(name, children?)`

Creates a custom rung handle, optionally containing nested rungs. Names accept
ASCII letters, numbers, `-`, and `_`. Nested children are prefixed with their
parent, so a child `second` inside `group` becomes `group.second`.

Container rungs order their children but cannot own actions.

### `w.ladder(name, rungs)`

Root-only. Selects the complete execution ladder.

Takes **handles**, not strings — it constructs the tree rather than referring to
it. Must contain every core event in its established order, and a handle can't
appear twice.

```lua
local configure = w.rung("configure")

w.ladder("workstation", {
    w.rungs.materialise.before,
    configure,
    w.rungs.materialise.tasks,
    w.rungs.materialise.artifacts,
    w.rungs.materialise.publish,
    w.rungs.materialise.after,
    w.rungs.deploy.before,
    w.rungs.deploy.apply,
    w.rungs.deploy.after,
})
```

Elsewhere — `at` and `when` — name a rung by string. That's also the only way to
reference a custom rung declared in another module.

## Data, processes, and logging

### `w.data.toml(path)`

Reads a TOML file from the repository and returns it as frozen Lua data. The
file's digest joins the source catalogue and build identity, so editing it
produces a new build.

```lua
local packages = w.data.toml("data/packages.toml")
```

### `w.exec(argv, options?)` · `w.shell(command, options?)`

Runs a process during construction to observe something. `w.exec` takes an argv
array; `w.shell` takes a command string.

- `options.cwd` — working directory.
- `options.env` — string map of environment variables.
- `options.stdin` — string written to the process's input.
- `options.timeout` — positive number of **seconds**; fractions are allowed.
- `options.max_output` — retained output limit in bytes, between 1 and
  67108864.
- `options.sensitive` — boolean. The observation still records status and
  digests, but `wombat inspect observations` shows the invocation as
  `<redacted>`.
- `options.shell` — absolute path to the shell. `w.shell` only.

Returns a table with `ok`, `code`, `signal`, `stdout`, `stderr`, and a `check()`
method that raises on failure and otherwise returns the result.

The command, its status, and output digests are recorded as a process
observation and participate in build identity. Output is bounded; exceeding
`max_output` is an error rather than silent truncation.

Use these to observe, not to change things. Mutation belongs in a script, at
execution time, where it's journalled.

```lua
local version = w.exec({ "python3", "--version" }):check().stdout
local branch = w.shell("git rev-parse --abbrev-ref HEAD"):check().stdout
```

### `w.log.debug|info|notice|warn|error(message, fields?)`

Emits a structured log line during construction.

- `message` — string.
- `fields` — optional table of structured context, rendered alongside the
  message.

Filtered by the repository's `log.level` and by `--log-level`, `-v`, and `-q`.
Logging is presentation only: it doesn't enter the manifest or affect identity.

```lua
w.log.info("configuring example", { mode = "canonical" })
```
