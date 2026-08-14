# Run tasks and scripts

Two ways to run your own code during a build, for two different jobs.

**Tasks** live in `tasks/` and produce artifacts. Think "generate this config
from a template engine I prefer" or "validate that the tree is sane".

**Scripts** live in `scripts/` and change the world. Think "rebuild the font
cache", "reload the shell config after deployment".

The distinction matters because tasks are cached and re-run when their inputs
change, while scripts are scheduled and stateful. Reach for a task when you're
producing content, and a script when you're causing an effect.

## Write a task

```lua
w.build.task("generate.py", { message = "Hello" })
w.build.task("validate.sh", {}, { cache = false })
```

Python, POSIX shell, Bash, embedded Lua 5.5, and direct executables are inferred
from the entrypoint. A direct executable needs a shebang — a text file marked
executable without one fails on Linux, even though macOS will quietly run it
under a shell.

Tasks run in a private build-local workspace and receive fixed arguments:

| Argument | Meaning |
| --- | --- |
| `--params` | your params, as JSON |
| `--output-dir` | regular files written here become artifacts |
| `--work-dir` | scratch, cleared before each execution |
| `--cache-dir` | task-private, persists between runs |
| `--source-dir` | the repository source |
| `--scope` | the execution scope |

Python tasks can skip the parsing:

```python
from wombat import params, output, work, cache, source, scope

(output / "app.toml").write_text(f"greeting = \"{params['message']}\"\n")
```

Anything left in `output` is published as an ordinary artifact. A task that
writes nothing is a perfectly good build gate — that's what `validate.sh` above
is.

Results are cached against the task's inputs, so an unchanged task doesn't run
again. `cache = false` opts out for gates that should always run. Caches live
inside the build directory and never enter the product.

## Write a script

```lua
w.script("configure.py", { profile = "desktop" }, {
    at = "configure",
    schedule = "onchange",
    files = { "helpers/**" },
})
```

Name the rung as a string. A string is checked against the ladder, so a typo is
reported; the typed handle works too, but it's what you use to *build* a ladder
rather than to point at one. A custom rung declared in another module can only
be named by string anyway.

Schedules:

| Schedule | Runs |
| --- | --- |
| `always` | every time the ladder reaches its rung |
| `once` | the first time, then never again |
| `onchange` | when its payload, params, options, or `files` digests change |

`--rerun-scripts` forces scheduled scripts without deleting their state, which is
what you want when something failed halfway and you'd rather not reason about it.
`--skip-scripts` takes the opposite path: Wombat does not resolve script
runners, prepare scheduling state, or execute scripts at all. Build tasks still
run. The two flags conflict, so contradictory intent fails at argument parsing.

Scripts get a private persistent cache, a fresh work directory, and their frozen
payload. Their output is forwarded live, attributed to the script, so a slow one
isn't silent:

```text
[<root>:configure.py:target:configure] configuring desktop profile
```

## Place them on the ladder

Tasks are artifact factories, so they may only sit on rungs up to artifact
construction. Scripts may sit at any leaf.

If you want a custom rung, the root selects the whole ladder, including every
mandatory core event in order. This is where handles are required — `w.ladder()`
takes handles, not strings, because it's constructing the tree rather than
referring to it:

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

Actions sharing a rung run in declaration order, and that order is frozen into
the plan.

## Inspect before running

```sh
wombat plan construct
wombat plan inspect tasks
wombat plan inspect scripts
wombat plan inspect ladder
```

Because construction freezes every action before any of them runs, you can read
the whole list — and its order — before a single one executes.

Afterwards, the execution journal records what actually happened:

```sh
wombat inspect ladder
wombat inspect scripts
```

When requirements or scripts are explicitly skipped, the same journal records
the skipped rung/action and its reason. Inspection therefore distinguishes an
intentional fast path from work that silently disappeared.

## Remember they're trusted code

Tasks and scripts run with your privileges. A Wombat repository is a program —
read it before running it, particularly one you didn't write.

Scripts scoped to the host during a compile-only build need explicit
`--allow-host-scripts`, because building for another target shouldn't quietly
run things on this one.
