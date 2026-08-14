# Render templates

When a file needs values in it — a colour from your theme, a path that differs
per machine — make it a template. Lua assembles the values; Rust renders them.

## Make a template

Give the source a `.tmpl` suffix and pass a context:

```lua
local w = require("wombat")
local theme = w.using("theme")

w.module.from(".config")
w.install("starship.toml", {
    with = {
        colors = theme.colors,
        shell = "zsh",
    },
})
```

`src/dot_config/starship.toml.tmpl` renders to `.config/starship.toml`. The
suffix is removed from the inferred target, and you refer to the file by its
target name.

The context is frozen when you declare it. Whatever Lua computed at that moment
is what gets rendered, and it's recorded in the manifest — which means
`wombat explain` can show you exactly what a file was rendered with.

## The template language

Rendering uses a versioned contract called `handlebars-v1`, backed by full
Handlebars: interpolation, `if`/`unless`/`each`/`with` (including `else`
blocks), comments, raw blocks, whitespace control, the built-in comparison and
logic helpers (`eq`, `ne`, `gt`, `gte`, `lt`, `lte`, `and`, `or`, `not`, `len`),
`lookup`, `log`, subexpressions, and inline partials/decorators.

Two things stay deliberately stricter than stock Handlebars, on every
construct that resolves a value — plain interpolation, `if`/`unless`
conditions, `each`/`with` targets, helper arguments: missing values are a hard
render error rather than silently empty or falsy, and nothing is HTML-escaped
(these are config files, not web pages). Dotted paths reach directly into
nested context (`{{theme.tmux.border.color}}`), so you can pass a whole table
and let the template consume whatever part of it it needs.

```handlebars
{{#if shell}}
shell = "{{shell}}"
{{/if}}

{{#each colors}}
{{@key}} = "{{this}}"
{{/each}}

{{#if (eq shell "zsh")}}
default_shell = "{{shell}}"
{{/if}}
```

## Whitespace

Handlebars keeps ordinary whitespace, including the newlines around block tags.
Write templates with the line structure you want in the output.

`~` trims adjacent whitespace, and it's sharper than it looks:

```handlebars
prefix={{~ value ~}}suffix
```

That deliberately removes whitespace on both sides, joining the text together. Used
carelessly it will join lines you wanted separate.

## Naming edge cases

If both `starship.toml` and `starship.toml.tmpl` exist, the relaxed lookup is
ambiguous and fails rather than picking one. Spell out the `.tmpl` to
disambiguate.

For unconventional names, be explicit:

```lua
w.install.file("literal.tmpl")                                    -- deploy the .tmpl file as-is
w.install.template("input", { to = ".config/output", with = ctx }) -- render a file not named .tmpl
```

Template directories, implicit runtime context, includes, and callbacks aren't
supported.

## When a template isn't enough

If you need real logic or an external tool to produce the content, that's a task,
not a template — see [run tasks and scripts](run-tasks-and-scripts.md). If Lua
can compute the bytes directly, publish them:

```lua
w.generate("starship.toml", {
    content = rendered_bytes,
    to = ".config/starship.toml",
})
```

Generated content behaves like any other artifact for ownership, identity,
inspection, and deployment.

## Don't put secrets in context

Frozen context is manifest data: readable by anyone who can read the product, and
part of its identity. Wombat has no secret management yet.

## Check the result

```sh
wombat build
wombat explain ~/.config/starship.toml
```

`explain` shows the frozen context, the template source digest, and the rendered
result's identity, which is usually enough to see why a value came out the way it
did.
