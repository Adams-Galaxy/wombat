# Wombat Example Dotfiles

This is a small but complete Wombat source repository. It selects Homebrew on
macOS and Apt on Debian-family Linux, declares portable command products, and
builds a harmless shell, Git, prompt, editor, and local-tool configuration. It
also demonstrates binary-safe Lua generation, a cached Python generator with a
companion module, and an uncached outputless validation task.

Module files are deliberately flat and organized as code; their physical
location has no relationship to deployed paths. Each module establishes any
source base it needs with `w.module.from()`, and template installs use target
names—the physical `.tmpl` suffix is compiler metadata, not authoring ceremony.
The root declaration also installs `.wombat-example`, demonstrating that
arbitrary leading-dot targets use the same generic convention as every other
path.

```sh
wombat -S examples/dotfiles deploy --target-root /tmp/wombat-root -- \
  --name "Example User" --email example@example.invalid
```

Use `wombat inspect`, `wombat check`, and `wombat bootstrap` against the
resulting build to explore the exact product.
