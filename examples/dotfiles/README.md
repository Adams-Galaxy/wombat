# Wombat Example Dotfiles

This is a small but complete Wombat source repository. It selects Homebrew on
macOS and Apt on Debian-family Linux, declares portable command products, and
builds a harmless shell, Git, prompt, editor, and local-tool configuration. It
also demonstrates binary-safe Lua generation, a cached Python generator with a
companion module, and an uncached outputless validation task.

```sh
wombat -S examples/dotfiles deploy --target-home /tmp/wombat-home -- \
  --name "Example User" --email example@example.invalid
```

Use `wombat inspect`, `wombat check`, and `wombat bootstrap` against the
resulting build to explore the exact product.
