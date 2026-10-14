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
tui-shim choose [APP]         # pick system vs user-installed copy (floating picker)
tui-shim edit [FILE] [--new]  # "Open with" editor picker, or start a new file
tui-shim niri-rule            # window-rule snippet that floats the picker windows
tui-shim doctor
```

## Several copies of one program
`~/.cargo/bin/yazi` next to `/usr/bin/yazi`: `scan`/`list` show every distinct copy (merged-usr symlinks are
deduplicated). Without a saved choice the first one in lookup order wins. `choose` stores your pick in
`~/.config/tui-shim/choices.toml`; `sync` keeps honouring it and never silently switches.
`sync` also writes two helper launchers: "TUI app versions" (the chooser) and "Edit in terminal..." which is
offered for text files in "Open with" (it does not take over any default; mimeapps.list is untouched) and has a
"New file..." action. Both run in a window with class `tui-shim.picker`.

Global: `--dry-run`, `--offline`, `--palette FILE` (or `TUI_SHIM_PALETTE`).
Default palette: `~/.config/futuro-darko/palette.md` (markdown tables of `name | #HEX`).

Apps are described in `registry/*.toml`; theme templates live in `templates/` with `{{palette_key}}` placeholders.
Config files tui-shim edits are backed up once as `*.bak-tui-shim`; files not created by it are skipped without `--force`.
