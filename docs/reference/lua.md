# Lua API reference

Everything below comes from `require("wombat")`, conventionally bound to `w`.

All of it runs at **construction time** — while the plan is being frozen. Nothing
here executes during materialisation or deployment. Where an entry says it
"records" something, that means it goes into the plan for Rust to execute later.

```lua
local w = require("wombat")
```

Contents: [selection](#module-selection) · [sources](#sources-and-artifacts) ·
[templates and generated content](#templates-and-generated-content) ·
[inputs](#inputs) · [context](#host-and-target-context) ·
[requirements](#requirements-and-providers) ·
[tasks and scripts](#tasks-and-scripts) · [ladders](#ladders) ·
[data, processes, logging](#data-processes-and-logging)

## Module selection

### `w.use(name, config?)`

Selects a module by filename stem from anywhere under `modules/`, optionally
configuring it. Modules are singletons: the first selection wins, and a second
selection with different configuration fails.

Fails if the module isn't found, its stem is ambiguous, or it forms a cycle.

```lua
w.use("theme", { name = "kanagawa" })
```

### `w.using(name)`

Selects the module if needed, evaluates it, and returns its frozen export.
Records a dependency from the calling module.

```lua
local theme = w.using("theme")   -- theme.colors is now readable
```

### `w.module.config()`

Returns the configuration table this module was selected with, frozen. Empty when
selected without configuration.

### `w.module.from(source, options?)`

Sets the module's source base, and by default the target base too. `options.to`
separates them when the two differ.

```lua
w.module.from(".config")                        -- src/dot_config -> .config
w.module.from("@staging", { to = ".config" })   -- source only; target is .config
```

## Sources and artifacts

### `w.install(source, options?)`

Declares one or more artifacts. `source` is an exact name, a directory, a glob,
or a `w.hidden()` value. Directories expand recursively into their regular file
leaves in deterministic order.

Options: `to` (explicit target path), `with` (template context, which also makes
it a template), `exclude` (string or array of glob exclusions), `allow_empty`
(boolean; permits a selector to match nothing).

A trailing `.tmpl` on the source makes it a template automatically and the suffix
is removed from the inferred target. Symlinks, special files, and — unless
`allow_empty` — empty selections are refused.

```lua
w.install("starship.toml")
w.install("nvim")
w.install("*.toml", { exclude = { "draft.toml" } })
w.install("themes/**", { allow_empty = true })
```

### `w.install.file(source, options?)`

Installs literally, never as a template. Use it for a real `.tmpl` file you want
deployed unrendered. Passing `with` is an error.

### `w.install.template(source, options?)`

Installs as a template even when the name doesn't end in `.tmpl`. Context
defaults to an empty table.

```lua
w.install.template("input", { to = ".config/output", with = context })
```

### `w.hidden(source)`

Selects a source entry whose real name begins with a dot. Without this, dotted
entries are invisible to selection.

```lua
w.install(w.hidden(".editorconfig"))
```

## Templates and generated content

### `w.generate(name, options)`

Publishes bytes assembled in Lua as an ordinary artifact, with normal ownership,
identity, inspection, and deployment behaviour. Binary-safe.

Options include `content` and `to`.

```lua
w.generate("starship.toml", {
    content = rendered_bytes,
    to = ".config/starship.toml",
})
```

Template rendering itself is Rust's job — see
[render templates](../how-to/render-templates.md) for the `handlebars-v1`
contract.

## Inputs

### `w.inputs(schema)`

Declares the repository's own command-line inputs and returns the resolved
values. Callers pass them after `--`. Unknown or invalid options fail before any
module evaluates.

```lua
local input = w.inputs({
    target = w.input.target(),
    theme  = w.input.choice({ values = { "gruvbox", "kanagawa" }, default = "gruvbox" }),
    yazi   = w.input.flag({ default = true }),
})
```

### `w.input.flag|choice|string|integer|target(options?)`

The input kinds. Common options are `default` and `help`; `choice` also takes
`values`. Flags accept `--name` and `--no-name`; value inputs accept both
`--name value` and `--name=value`.

## Host and target context

### `w.host`

Immutable observed facts about this machine: OS name and version, kernel,
Linux distribution identity, architecture, hostname, username, home.

### `w.target`

The resolved deployment target, starting as the normalised local OS and
architecture. Root configuration may replace it once, before anything reads it.

Only facts you actually read enter the manifest, and they participate in build
identity.

```lua
if w.target.os.name == "macos" then w.use("macos-only") end
if w.host.os.distribution and w.host.os.distribution.id == "fedora" then
    w.use("fedora")
end
```

### `w.paths.repository`

Absolute path to the source repository.

## Requirements and providers

### `w.need.command(name, options?)` · `w.need.package(name, options?)`

Declares something that must be available. `need` is required; a failure to
satisfy it fails the build.

Options: `minimum` (version), `when` (a rung deadline, defaulting to
`materialise.before`), `accept` (alternatives that also satisfy it).

```lua
w.need.command("python3", { minimum = "3.12", when = w.rungs.materialise.tasks })
```

### `w.prefer.command(name, options?)` · `w.prefer.package(name, options?)`

Same shape, but optional: an unsatisfiable `prefer` is reported, not fatal.

```lua
w.prefer.command("rg", { accept = { "grep" } })
```

### `w.providers(entries)`

Root-only. Selects the ordered providers that may satisfy requirements. Built-ins
are Homebrew (macOS) and Apt (Debian-family Linux); custom providers are ordinary
tracked Lua.

```lua
w.providers({ "brew" })
```

## Tasks and scripts

### `w.build.task(entrypoint, params?, options?)`

Declares a program under `tasks/` that generates artifacts, or acts as an
outputless build gate. Tasks are artifact factories and may only be placed on
rungs up to artifact construction.

Options include `at` (rung), `cache` (boolean).

```lua
w.build.task("generate.py", { message = "Hello" })
w.build.task("validate.sh", {}, { cache = false })
```

### `w.script(entrypoint, params?, options?)`

Declares a program under `scripts/` as a stateful ladder action. Scripts may run
at any leaf rung, and are not artifact factories.

Options include `at` (rung), `name`, `schedule` (`always`, `once`, `onchange`),
`files` (globs whose digests drive `onchange`), `scope`, `timeout`,
`interpreter`, `logs`.

```lua
w.script("configure.py", { profile = "desktop" }, {
    at = configure,
    schedule = "onchange",
    files = { "helpers/**" },
})
```

Both infer Python, POSIX shell, Bash, embedded Lua 5.5, or a direct executable
entrypoint. See [run tasks and scripts](../how-to/run-tasks-and-scripts.md) for
the calling contract.

## Ladders

### `w.rungs`

The eight mandatory core events, as typed handles:
`materialise.before`, `materialise.tasks`, `materialise.artifacts`,
`materialise.publish`, `materialise.after`, `deploy.before`, `deploy.apply`,
`deploy.after`. Read-only; canonical strings normalise to the same IDs.

### `w.rung(name, children?)`

Creates a custom rung handle, optionally containing nested rungs. Names accept
ASCII letters, numbers, `-`, and `_`. Container rungs order their children but
cannot own actions.

### `w.ladder(name, rungs)`

Root-only. Selects the complete execution ladder. It must contain every core
event in its established order; a handle can't be reused.

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

## Data, processes, and logging

### `w.data.toml(path)`

Reads and freezes a TOML file from the repository as Lua data. The file's digest
becomes part of build identity.

### `w.exec(argv, options?)` · `w.shell(command, options?)`

Runs a process during construction to observe something, returning a value with
`ok`, `code`, `signal`, `stdout`, `stderr`, and a `check()` helper that raises on
failure.

Options include `stdin`, `timeout_ms`, `max_output`, and environment control.
Output is bounded and the result — command, status, and output digests — is
recorded in the manifest as an observation.

Use these to observe, not to mutate. Mutation belongs in a script at execution
time.

```lua
local version = w.exec({ "python3", "--version" }):check().stdout
```

### `w.log.debug|info|notice|warn|error(message, fields?)`

Emits a structured log line at construction time, with optional structured
fields. Filtered by the repository's `log.level` and by `--log-level`, `-v`, and
`-q`.

```lua
w.log.info("configuring example", { mode = "canonical" })
```
