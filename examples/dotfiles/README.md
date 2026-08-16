# Wombat Example Dotfiles

This is a small but complete Wombat source repository. It selects Homebrew on
macOS and Apt on Debian-family Linux, declares portable command products, and
builds a harmless shell, Git, prompt, editor, and local-tool configuration. It
also demonstrates binary-safe Lua generation, a cached Python generator with a
companion module, an uncached outputless validation task, a custom execution
ladder, a scheduled Python preparation script, and a post-deployment shell
verification script.
Its editor template also registers a deterministic Lua Handlebars helper pack
and derives a translucent color from the theme's small base palette.
The shell module snapshots `w.os` and `w.paths` through
`w.template.context()`, keeping lazy construction views out of the persisted
template payload.

Module files are deliberately flat and organized as code; their physical
location has no relationship to deployed paths. Each module establishes any
source base it needs with `w.module.from()`, and template installs use target
names—the physical `.tmpl` suffix is compiler metadata, not authoring ceremony.
The root declaration also installs `.wombat-example`, demonstrating that
arbitrary leading-dot targets use the same generic convention as every other
path.

```sh
wombat -S examples/dotfiles build -- \
  --name "Example User" --email example@example.invalid
```

Use `wombat plan inspect ladder`, `wombat plan inspect scripts`, `wombat
inspect ladder`, `wombat inspect scripts`, and `wombat check` against the
resulting build to explore the stored plan and exact product. `wombat apply`
constructs, materialises, and deploys the complete example workflow.
