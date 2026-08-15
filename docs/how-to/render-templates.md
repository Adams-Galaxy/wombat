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
`lookup`, `log`, subexpressions, and inline partials/decorators, plus one
Wombat-specific addition: `coalesce`, which returns the first param that
is neither missing nor null, rendered with ordinary Handlebars interpolation
semantics. It is a value-returning fallback, not `or`'s boolean result:
`false`, `0`, and `""` are deliberate values and therefore win. Calling it
without arguments, or with only missing/null arguments, is an error.

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

border_color = "{{coalesce tmux.border.color generic.border.color palette.bright_black}}"
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

Template directories, implicit runtime context, and external partial includes
aren't supported.

## Add a reusable Lua helper

Projection logic that repeats across many templates can live in a deterministic
Lua helper pack. Keep the base palette small and derive presentation variants at
the point of use:

```lua
-- lua/theme/colors.lua
local function parse(color)
    local red, green, blue = color:match("^#(%x%x)(%x%x)(%x%x)$")
    assert(red, "expected #RRGGBB")
    return tonumber(red, 16), tonumber(green, 16), tonumber(blue, 16)
end

local function hex(value)
    return string.format("%02x", math.floor(value + 0.5))
end

return {
    alpha = function(color, amount, options)
        assert(amount >= 0 and amount <= 1, "alpha must be between 0 and 1")
        return color .. hex(amount * 255) .. (options.suffix or "")
    end,

    mix = function(left, right, amount, options)
        local lr, lg, lb = parse(left)
        local rr, rg, rb = parse(right)
        local function channel(a, b) return hex(a + (b - a) * amount) end
        return "#" .. channel(lr, rr) .. channel(lg, rg) .. channel(lb, rb)
    end,

    is_dark = function(color, options)
        local red, green, blue = parse(color)
        return red + green + blue < 384
    end,
}
```

Register it once from root configuration or a selected module:

```lua
w.template.helpers("theme.colors", { prefix = "color_" })
```

Then use its exports as ordinary Handlebars value helpers:

```handlebars
background = '{{color_alpha theme.background 0.6 suffix="cc"}}'
border = "{{color_mix theme.background theme.surface 0.5}}"
mode = "{{#if (color_is_dark theme.background)}}dark{{else}}light{{/if}}"
```

The helper source and every dependency loaded by top-level `require()` are
frozen into the plan and participate in identity and template-cache keys. A
fresh constrained Lua state is created lazily only when an uncached template
actually calls a custom helper. There is no built-in color policy: these names
and transformations belong to the repository.

## When a template isn't enough

If you need filesystem or process access, nondeterministic behavior, or an
external tool to produce the content, that's a task, not a helper — see
[run tasks and scripts](run-tasks-and-scripts.md). If configuration-time Lua can
compute the bytes directly, publish them:

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
