# Ownership and deployment

Wombat only touches files it owns. Everything else in your home directory is none
of its business, and it will not remove, rewrite, or tidy files it didn't put
there.

## The three-way comparison

Before changing anything, Wombat looks at three things:

1. **Last applied state** — what it deployed the previous time, kept privately
   under `$XDG_STATE_HOME/wombat/targets/`.
2. **The target now** — what's actually on disk.
3. **The desired product** — the verified build you're deploying.

Comparing all three is what makes the difference between "this file changed
because I changed my config" and "this file changed because you edited it by
hand". A two-way diff can't tell those apart.

From that comparison each artifact gets an action:

| Action | When |
| --- | --- |
| Create | the target doesn't exist |
| Update | Wombat owns it and the content should change |
| Adopt | the target already matches the desired content exactly |
| Remove | Wombat owned it, and it's no longer declared |
| Forget | it was owned, but has already been deleted |
| AdvanceState | nothing to do on disk; only the record needs updating |
| Conflict | something needs your decision |

Creates, updates, adoptions, and stale removals are safe and happen without
asking. Neighbouring files you never declared are untouched.

## Conflicts

You get a conflict when an unmanaged file already occupies an artifact's target
path, or when you've edited a managed file since it was deployed. Wombat won't
pick for you:

```sh
wombat apply --conflict ask        # prompt (the default on a terminal)
wombat apply --conflict fail       # stop on the first one
wombat apply --conflict skip       # leave them, deploy the rest
wombat apply --conflict overwrite  # take the product's version
```

`ask` needs a terminal. In a script, deployment fails rather than assuming, so
automation has to state its policy up front. `skip` succeeds with a deliberately
incomplete target — it's honest about that rather than pretending it converged.

There's no broad `--force`. Every escape hatch is per-conflict and named.

## Nothing changes before you've decided

Deployment computes the whole picture — including checked provider prerequisites,
shared preparation, and package work — and collects every decision before it
mutates anything. If you decline a conflict, no repository, package, or key was
installed and no file was written. If a package provider fails, the deployment
target is untouched.

That ordering is deliberate and tested. A half-applied deployment where the
packages landed but the files didn't is the failure mode this design exists to
prevent.

## What it won't claim

Files are replaced atomically one at a time, so an interrupted deployment leaves
each individual file either old or new, never truncated. But Wombat does not
claim whole-deployment rollback: if it stops halfway, some artifacts are updated
and some aren't. Run it again — deployment is idempotent, and the next run
reconciles the remainder.

## Target roots and platforms

A manifest path like `.config/app.toml` always means literally
`<target-root>/.config/app.toml`. It does not consult `XDG_CONFIG_HOME` on the
deploying machine, because the product has to mean the same thing everywhere.

Deploying implicitly to your own home refuses a product whose target OS doesn't
match the machine you're on. Passing `--target-root` explicitly is the deliberate
way out, which is what makes testing into a scratch directory easy:

```sh
wombat diff --target-root /tmp/proof
wombat apply --target-root /tmp/proof --conflict fail --yes
```

An architecture mismatch warns but doesn't block.

## Trust

Repository Lua, tasks, scripts, and custom providers are ordinary code that runs
on your machine. Read a Wombat repository before you run `build`, `apply`, or
`setup` on it, exactly as you would any other program. The guarantees on this
page are about managed deployment behaviour, not a sandbox.

Frozen template context and persisted plans are inspectable data. Don't put
secrets in them.
