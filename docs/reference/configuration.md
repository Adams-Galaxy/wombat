# Configuration reference

Two configuration files, both optional, plus a handful of environment variables.

## Repository config: `wombat.toml`

Sits at the root of your source repository and describes the repository itself.
Unknown keys are rejected, so a typo fails loudly.

```toml
format_version = 3
project = "dotfiles"

[artifacts]
unallocated = "warn"

[log]
level = "warn"

[workflow]
reuse = true
freshness = "5m"
```

| Key | Values | Default | Meaning |
| --- | --- | --- | --- |
| `format_version` | `3` | required | Config format. Mismatches are rejected |
| `project` | 1–64 chars of ASCII letters, digits, `-`, `_` | unset | Names this project's script state |
| `artifacts.unallocated` | `ignore`, `warn`, `error` | `warn` | What to do about source files no declaration selected |
| `log.level` | `debug`, `info`, `notice`, `warn`, `error` | `warn` | Minimum level for optional log output |
| `workflow.reuse` | boolean | `true` | Allow reusing a fresh matching product |
| `workflow.freshness` | duration, e.g. `5m` | `5m` | How long a product counts as fresh for reuse |

`init` writes this file for you.

### About `project`

Scheduled scripts keep state between runs — that's how `once` runs once and
`onchange` notices changes. That state has to be namespaced per project.

Without `project`, the namespace follows the repository's location on disk, so
moving your checkout starts the state fresh and `once` scripts run again.
Declaring `project` names the namespace instead, so state survives relocation,
and two checkouts declaring the same name share it deliberately.

It does not affect build identity.

## User config: `config.toml`

Your own settings, not the repository's. Wombat looks in
`$XDG_CONFIG_HOME/wombat/config.toml`, falling back to
`~/.config/wombat/config.toml`.

```toml
format_version = 2
repository = "~/dotfiles"

[runners.python]
command = "~/.venvs/wombat/bin/python"
args = []
```

| Key | Meaning |
| --- | --- |
| `format_version` | `2`. Required |
| `repository` | Default source repository when `--source` isn't given |
| `runners.<name>.command` | Interpreter to use for that runner family |
| `runners.<name>.args` | Extra arguments passed before the entrypoint |

Without this file and without `--source`, the source defaults to
`~/.local/share/wombat/`.

## Environment variables

| Variable | Used for |
| --- | --- |
| `HOME` | Default target root, default config and state locations |
| `XDG_CONFIG_HOME` | User config location; falls back to `~/.config` |
| `XDG_STATE_HOME` | Deployment and script state; falls back to `~/.local/state` |
| `NO_COLOR` | Honoured by `--color auto` |
| `PATH` | Locating interpreters, providers, and Git |
| `USER` | Host observation only |

Tasks and scripts additionally receive `PYTHONPATH` pointing at Wombat's Python
helper when their runner is Python.

The installer reads `WOMBAT_INSTALL_REPOSITORY`, `WOMBAT_INSTALL_REV`, and
`WOMBAT_INSTALL_ROOT`.

A manifest path such as `.config/app.toml` always means literally
`<target-root>/.config/app.toml`. It does not consult `XDG_CONFIG_HOME` on the
deploying machine.

## Where state lives

| Path | Contents |
| --- | --- |
| `$XDG_STATE_HOME/wombat/targets/<target>/` | Last-applied deployment state and journal |
| `$XDG_STATE_HOME/wombat/scripts/materialise/<project>/` | Script scheduling state |
| `<build>/.wombat/` | Plan, locks, caches, staging, recovery state |

Everything under `.wombat/` is working state and never travels with a product.
See [the formats reference](formats.md) for the files themselves.
