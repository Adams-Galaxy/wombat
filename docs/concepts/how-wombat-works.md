# How Wombat works

Wombat is a compiler. Your Lua is the source, an executable plan is the
intermediate representation, and a verified product is the output. Deployment is
a separate, guarded step at the end.

There are three stages, and they always happen in this order.

## Construct

`plan construct` evaluates your Lua exactly once. It resolves modules, reads
inputs, observes the host, picks providers, and works out every artifact,
requirement, checked provider prerequisite, task, and script involved.

The result is frozen into an executable plan on disk. That plan is the complete
list of things Wombat is permitted to do. Nothing outside it can happen later,
so a script can't appear halfway through a build because a condition changed.

This stage is passive. It reads your repository and observes your machine, but
it doesn't install packages, run your tasks, or write to your home directory.

## Materialise

`plan materialise` executes that exact plan, and does not run your configuration
Lua again. It checks and reconciles provider prerequisites before their dependent
requirements, performs repository-dependent availability checks only after the
prerequisite passes its post-check, reconciles requirements at their declared
deadlines, runs tasks to generate content, renders templates, and assembles a
complete tree of the files that would be deployed.

The output is a *product*: a `manifest.json` describing everything, plus a
`tree/` holding the bytes. It's staged, verified, and only then published, so an
interrupted build leaves either the previous product or a recoverable staged
one — never a half-written product that looks finished.

## Deploy

`plan deploy` takes one verified product and reconciles it with a target root,
normally your home directory. This is the only stage that touches files outside
the build directory.

It works in two phases. First it observes: it reads the target, computes what
would change, collects warnings, and asks you about anything ambiguous. Only
once every decision has been made does it start changing things. Declining a
conflict means no package was installed and no file was written.

## The commands you'll actually use

`build` runs construct and materialise. `apply` runs all three. `setup` adds
repository acquisition in front of `apply`, for a machine that has nothing yet.

The individual `plan` subcommands exist for when you want to stop and look
between stages, which is worth doing whenever something surprises you.

```sh
wombat plan construct   # freeze the intent
wombat plan inspect     # read it
wombat plan materialise # build the product
wombat diff             # see what deployment would do
wombat apply            # do it
```

## Construction time and execution time

This distinction runs through everything, and it's the one worth internalising.

Construction time is Lua. Conditionals, loops, reading host facts, choosing
modules, assembling template context — all of it happens while the plan is being
frozen, and all of it is recorded.

Lazy Wombat namespaces stay lazy until read. When a template needs a complete
namespace, `w.template.context({ os = w.os, paths = w.paths })` snapshots it
recursively into ordinary plan data during construction; the renderer never
receives a live Lua or host-context handle.

Execution time is Rust. Running tasks, rendering templates, installing packages,
writing files. By then your Lua has finished and the decisions are already made.

So `if w.macos then ... end` is a construction-time choice, and
the plan records which branch you took. There's deliberately no way to make that
decision later, during execution, because then the plan would no longer describe
what actually happened.

## Why freeze at all

Because it makes the interesting questions answerable before anything changes.
You can read the plan, diff the product against your machine, and explain where a
single file came from — all without running your configuration again and all
without having deployed anything.

It also means the thing that gets deployed is the thing you inspected. There's no
second evaluation that might resolve differently.
