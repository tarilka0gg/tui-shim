# tui-shim

Scans for known TUI apps (btop, htop, yazi, micro, nvim, tmux, ...), then for each one:

- writes a launcher `.desktop` entry that opens it in your terminal with its own window class
- finds an icon: installed icon themes first, then Dashboard Icons / simple-icons, then a generated glyph
- recolors it from your system palette (per-app theme file where the app supports one, terminal ANSI override otherwise)

```
tui-shim scan [--unknown]     # what's installed (--unknown: heuristic scan for unlisted ncurses apps, slow)
tui-shim sync [APP] [--force] # entries + icons + themes (idempotent)
tui-shim theme [APP]          # themes only
tui-shim clean                # remove only entries/icons it created
tui-shim doctor
```

Global: `--dry-run`, `--offline`, `--palette FILE` (or `TUI_SHIM_PALETTE`).
Default palette: `~/.config/futuro-darko/palette.md` (markdown tables of `name | #HEX`).

Apps are described in `registry/*.toml`; theme templates live in `templates/` with `{{palette_key}}` placeholders.
Config files tui-shim edits are backed up once as `*.bak-tui-shim`; files not created by it are skipped without `--force`.
