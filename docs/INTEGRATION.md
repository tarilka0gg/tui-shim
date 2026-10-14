# tui-shim: handoff for integration into Simple Linux / the ISO

Written 2026-10-14 by the agent that built it, for the agent that will package and integrate it.
Everything here was checked against the code and the running system on that date.
Repo: `~/Documents/projects/tui-shim` (local git only, never pushed; 32+ commits, author
`tarilka0gg <216761460+tarilka0gg@users.noreply.github.com>`, **no Claude attribution lines** - the owner's standing rule).
Version 0.1.0, Rust edition 2024 (built with rustc 1.96.1), 30 unit tests pass (`cargo test`).

---------------------------------------------------------------------------------------------------

## 1. What it is, in one paragraph

`tui-shim` makes terminal ("TUI") programs behave like normal desktop apps on a niri/Wayland desktop.
Two independent features share one Rust binary:

1. **Launcher integration** - scans for known TUI apps (btop, htop, yazi, micro, nvim, tmux, nmtui, nvtop,
   lazygit, ncdu), writes `.desktop` entries that open them in the user's terminal with their own window class,
   finds icons (installed icon themes first, then the web, then a generated glyph) and can recolour each app from the
   system palette.
2. **Editor file picker** (the part that took most of the work) - when a text editor (micro, nvim) runs in a terminal,
   a small round folder **badge** is drawn inside the terminal window, at its top-right corner. Clicking it (or F9)
   opens a native libadwaita file browser (grid of icons, Thunar-style bar). Choosing a file opens it **in the
   same editor window** by typing the editor's own "open file" command into it. When an editor starts with no file the
   picker opens by itself.

Nothing runs as a daemon. Everything is on demand and started by the editor launch.

---------------------------------------------------------------------------------------------------

## 2. How the editor picker works (the important part)

### 2.1 Entry points

| User action | What runs |
|---|---|
| types `micro file` / `nvim` in a terminal | `~/.local/bin/micro` is a **symlink to `tui-shim`**; the binary sees `argv[0] == "micro"` and goes to `run_shim()` |
| clicks the launcher entry "micro" | `Exec=tui-shim open micro %f` (`Terminal=false`) -> opens a new ghostty window running `tui-shim pty-run ... -- /usr/bin/micro` |

`run_shim()` hands straight to the real editor (`exec`) when stdin/stdout are not both a tty, when any argument starts with
`-`, or when there is more than one argument. So scripts, pipes, `git commit` with flags etc. are not affected.
Interactive plain use goes to `run_open()` -> `run_here()` (already inside a terminal) or spawns a terminal (launcher path).

### 2.2 The pty wrapper (`src/pty.rs`) - the heart

The editor does **not** run directly on the terminal. `pty::run()` opens a pty, runs the editor on the slave side and
bridges it to the real terminal. Because it owns the pty it can:

* **draw the badge**: kitty graphics protocol (APC `ESC _ G ... ESC \`), placed over a block of cells in the top-right
  corner, z-index 1000. Works in ghostty (tested 1.1.3) and should in kitty (untested). It is re-placed when idle, on resize
  (immediately, about 5 ms), after the editor switches screens, and every 1 s; the picture is re-uploaded when the editor
  sends a kitty "delete" command (`a=d`; snacks.nvim in LazyVim does) and at least every 3 s.
* **read mouse reports** (SGR 1006) and keystrokes the terminal sends to the editor, and **filter** them:
  a click on the badge cells is swallowed and reported as `click`; F9 (`ESC [ 20 ~`) as `key`; any other button press or
  any typed key (terminal replies to queries are excluded) is reported as `away`. The reports go as datagrams to the
  **control socket** `$XDG_RUNTIME_DIR/tui-shim-<pid>.ctl`.
* **type into the editor**: datagrams on `$XDG_RUNTIME_DIR/tui-shim-<pid>.sock` are written to the pty master. Opening a
  file = sending the editor's open command (see the registry, section 4).
* answer a **probe request** (section 2.4).

### 2.3 Overlay, tracker, panel (`src/gui.rs`, `src/track.rs`)

* `track.rs` runs `niri msg -j event-stream` in a thread and turns niri's window state into messages:
  `Show{right,top,edge_x,exact,monitor,id}`, `Hide`, `Quit`. The overlay process also receives `Click(Instant)`,
  `Hotkey(Instant)`, `Away(Instant)` from the control socket (stamped on **arrival**, which matters, see 6.7).
* `panel_overlay()` is a GTK4 + libadwaita application that holds no window between clicks. On a click it creates a small
  **layer-shell** surface (`gtk4-layer-shell`, layer Overlay, keyboard mode OnDemand), anchored near the editor window,
  containing the picker (`file_panel()`): navigation bar (back, forward, up, home, recents, path entry, search toggle),
  a stack of two flow-box pages that slide into each other, type-to-filter, Alt+arrows, Ctrl+L/F/H.
* When a file is chosen the panel closes, `on_pick` sends the open command to the wrapper's `.sock`, and the editor opens
  it in place. micro: `Ctrl-E` then `open "<file>"` + Enter. nvim: `Esc` then `:e <file>` + Enter (`<file>` quoted per editor).
* In terminal mode the wrapper runs in a **thread** of the same process as the overlay (`run_here`), with stderr redirected to
  `/dev/null` (or `$TUI_SHIM_LOG`) because GTK/Mesa warnings would otherwise be drawn over the editor.
  In launcher mode the wrapper is a separate process (`pty-run`, hidden subcommand) and the orchestrator
  (`tui-shim open`) hosts the overlay.

### 2.4 Where does the panel go? (niri gives no position for tiled windows)

`niri msg -j windows` returns `tile_pos_in_workspace_view = null` for every **tiled** window, only floating ones have a
position. So placement is a decision tree (`track::state_for`, `gui::Place`):

1. **Floating window** -> exact from niri.
2. **Tiled, and all columns of the workspace fit on the monitor** -> computed from column widths and the gap
   (`tiled_pos`; the gap is inferred when the columns fill the screen and remembered in `~/.config/tui-shim/gap`).
3. **Anything else (the strip is scrolled - very common)** -> niri does not report the scroll offset, a window can even sit
   with empty space beside it. **Never guess.** Use the **probe**: the overlay sends `PROBE` to the wrapper, which redraws
   the badge for a moment with the fill colour changed by +3 in blue (invisible), answers on a reply socket when it has
   drawn it, and the overlay takes one screenshot with **`grim`**, then looks for that exact colour. Candidates are grouped
   into connected blobs and the one that is badge-shaped is chosen (wallpapers can contain the same colour; `probe.rs`
   has tests for this). Takes about 90-200 ms.
4. No `grim` / probe fails -> centred near the top of the monitor.

The panel itself is placed 50 px inside the grid's right and top edges. The probe path anchors to the **output**
(`exclusive_zone -1`, margins from the output corner); the estimate path anchors to the usable area (bar excluded).

### 2.5 Closing rules (`gui.rs`, search for `Msg::Away`)

* click on the badge again: closes, but a second click within **250 ms** of the opening click is a double click and ignored;
* a click or keystroke that reaches the **editor** (`Away`): closes (ignored within 400 ms of opening);
* keyboard focus leaves the panel: if it went to the **editor's own window** the panel stays (new windows get focus late,
  focus-follows-mouse...), if to **another window** it closes, if within 350 ms of a badge click it takes the focus back
  (`KeyboardMode::Exclusive` for 250 ms);
* Esc inside the panel, or picking a file.

State remembered across panels, editor windows and restarts: `~/.config/tui-shim/view.toml` (folder + scroll offset),
`recent.toml` (30 recent files; the last one is revealed and selected when the panel reopens).

---------------------------------------------------------------------------------------------------

## 3. Source map

| File | Lines | Role |
|---|---|---|
| `src/main.rs` | ~730 | CLI (clap), shim detection by argv0, `run_open`, `run_here`, `run_shim`, `shims` command, control-socket listener, small append-only log |
| `src/gui.rs` | ~1230 | libadwaita pieces: `panel_overlay` (used with the in-terminal badge), legacy `overlay` (layer-shell button, used in terminals that cannot draw images), `file_panel` (the picker), `Grid`/`Slide`, CSS, `usable_areas` (debug) |
| `src/pty.rs` | ~640 | pty wrapper, badge geometry (cells, not pixels), input filter, probe answering, kitty graphics upload/place |
| `src/track.rs` | ~240 | niri IPC: window state, tiled position estimate, focused window, own-window lookup |
| `src/probe.rs` | ~210 | screenshot based badge locator (marker colour, blob search, reply socket) |
| `src/files.rs` | ~220 | listing, fuzzy filter, recents, persisted view state |
| `src/icon.rs` | ~120 | draws the badge picture (RGBA, no image crate), base64, marker colour |
| `src/registry.rs` + `registry/*.toml` | | one TOML per known app; `open_keys` for editors |
| `src/scan.rs`, `desktop.rs`, `icons.rs`, `theme.rs`, `palette.rs`, `choices.rs`, `picker.rs` | | launcher generation, icon lookup/download, per-app theming, palette parser, saved copy choices, terminal (TUI) fallback pickers |
| `templates/` | | theme templates for btop, micro, yazi with `{{palette_key}}` placeholders |

Cargo feature `gui` (default) pulls gtk4, libadwaita, gtk4-layer-shell. Without it everything except the picker still builds
(editors then fall back to a terminal prompt for the file name).

---------------------------------------------------------------------------------------------------

## 4. The registry (how to add an editor)

`registry/<name>.toml`. Fields used by the picker: `categories` must include `"TextEditor"`; `open_keys` is what is typed into
the running editor, `{file}` replaced by the quoted path; `open_quote` is `"shell"` (double quotes) or `"vim"` (backslash-escape);
`builtin_picker = true` means the editor has its own browser (now only informational: the picker auto-opens for every editor
started without a file). Without `open_keys` an editor still gets the badge but a chosen file opens in a **new window**.

```toml
# micro                      # nvim
open_keys  = "\u0005open {file}\r"     open_keys  = "\u001b:e {file}\r"
open_quote = "shell"                   open_quote = "vim"
```

Only **micro** and **nvim** are installed on the reference machine, so they are the only ones with `open_keys` and shims.
`hx` (`\u001b:open {file}\r`) and `vim` (`\u001b:e {file}\r`) would be one-liners; nano has no "open" command.

---------------------------------------------------------------------------------------------------

## 5. Dependencies

Build: Rust >= 1.85 (edition 2024; 1.96.1 used), a C compiler (the `ring` crate), `pkg-config`, GTK4 dev files, libadwaita dev files,
gtk4-layer-shell dev files. Crates: gtk4 0.11 (`v4_12`), libadwaita 0.9 (`v1_5`), gtk4-layer-shell 0.8, clap, crossterm,
ureq (rustls), serde/toml/serde_json, libc, include_dir, anyhow. Release binary ~5.6 MB.

Runtime (versions on the reference machine):

| Needed for | Package | Version |
|---|---|---|
| picker window | gtk4, libadwaita | 4.20.4, 1.8.6 (>= 1.5 needed) |
| panel placement | **gui-libs/gtk4-layer-shell** | 1.1.1-r1 |
| window tracking, focus | niri (IPC via the `niri msg` command) | 26.04 |
| badge detection probe | **grim** | present |
| in-terminal badge | **ghostty** (1.1.3) or kitty (untested) - needs the kitty graphics protocol | |
| icons (optional) | Papirus icon theme; otherwise generated glyphs | |
| desktop db | `update-desktop-database` (called by `sync`) | |

**gtk4-layer-shell is not in the official Gentoo binhost** (only an ebuild). A binary package was built on the reference machine
with `emerge --buildpkg`: `/var/cache/binpkgs/gui-libs/gtk4-layer-shell/gtk4-layer-shell-1.1.1-r1-1.gpkg.tar` (70 KB).
Put it in the Simple Linux binhost (`simple-linux-binhost`, signed) or build it in the ISO build chroot.

---------------------------------------------------------------------------------------------------

## 6. Things you must remember (hard-won; each cost real debugging time)

1. **ghostty joins everything after `-e` into one `sh -c` string without quoting.** A `#`, `;` or space in an argument is
   reinterpreted by the shell. Use `desktop::finish_args` / `sh_quote` for anything after `-e`. (This also fixed the nmtui
   `NEWT_COLORS` entry, which contains `;`.)
2. **ghostty reports terminal pixel sizes inflated by the display scale squared** (1.25 scale -> 2444x1854 px for a ~1568x1191
   window), while kitty images are drawn in real pixels. Never mix them. The badge is sized and positioned in **whole cells**
   (4 columns x 3 rows; only the cell height:width *ratio* is taken from the pixel numbers, the scale cancels). The click area is
   exactly those cells.
3. **kitty graphics storage is per screen.** An image uploaded on the main screen does not exist on the alternate screen
   (editors use it). The wrapper tracks `?1049h/l` and uploads again after a switch. Editors may also delete all images
   (`a=d`), detected and handled; plus a 3 s safety re-upload.
4. **Layer-shell keyboard mode:** `OnDemand` on a surface that is shown while the user is typing in the editor steals the
   keyboard focus. The panel only exists while open; the legacy button uses `KeyboardMode::None` when collapsed.
   Taking and returning focus makes nvim fire FocusLost/FocusGained (LazyVim repaints = flicker).
5. **Do not block the GTK main thread on `niri msg`** (an early version did that while resizing; the window could not
   acknowledge the resize and the size never landed). Long things run in threads (probe, tracker).
6. **A wrapper write to the pty can block** if the terminal is not reading (only in test harnesses). Tests that use a pty must
   keep reading while waiting.
7. **Double click:** building the panel the first time (icons for dozens of files) can take longer than the double-click
   window, and the queued second click would then look like "close". All clicks are stamped with their **arrival** time and
   compared by arrival time, not handling time.
8. **The wrapper must never leak its editor:** the child gets `PR_SET_PDEATHSIG(SIGHUP)`. (During testing 87 orphaned editors had
   piled up.)
9. **Shim symlinks must be symlinks to the `tui-shim` binary.** `scan::variants()` skips any candidate whose canonical path equals
   the running executable; a *copy* or hardlink of the binary named `micro` would not be skipped and would recurse.
   Shim mode triggers for any `argv[0]` not starting with `tui-shim`, so the binary itself must be called `tui-shim*`.
10. The default palette path is **hard-coded**: `~/.config/futuro-darko/palette.md` (`main.rs`, in `main` and `run_shim`);
    override with `--palette` / `TUI_SHIM_PALETTE`. Required keys: `bg`, `fg`, `accent`. Used by the GUI/badge: `bg`, `bg_alt`,
    `fg`, `accent`, `bg_paper`, `secondary`. Theme templates use more (`khaki`, `hills`, `yellow_dark`, ...). Format: markdown
    tables, rows `| name | #RRGGBB | ...` (an `## ANSI` section becomes `ansi_*` keys).
11. **Do not edit or read the user's niri config.** The owner rejected it twice. Floating/placement is done through IPC; the
    panel is a layer-shell surface and needs no window rule. (`tui-shim niri-rule` only prints an optional snippet.)
12. `sync` **without `--no-theme` overwrites** the user's active btop/micro/yazi theme settings (backups `*.bak-tui-shim`).
    Use `sync --no-theme` unless the ISO deliberately ships its own theming.
13. The probe grabs a full-output screenshot in memory with `grim` for ~100 ms and scans it for a colour; nothing is stored or
    sent. Mention it if privacy is ever asked.
14. Logging: `~/.cache/tui-shim/overlay.log` (append-only, capped at 200 KB) records clicks, probe steps, focus decisions.
    It is the first place to look when something misbehaves.

---------------------------------------------------------------------------------------------------

## 7. Files and sockets at runtime

| Path | Purpose |
|---|---|
| `~/.local/bin/tui-shim` (reference install) | the binary; launchers' `Exec=` use the path of the binary that ran `sync` |
| `~/.local/bin/micro`, `nvim` | shims (symlinks to the binary) created by `tui-shim shims install` (only `~/.local/bin`, hard-coded) |
| `~/.local/share/applications/tui-shim-<app>.desktop` | generated launchers, marked `X-TuiShim=true` (only marked files are ever touched/removed) |
| `tui-shim-choose.desktop`, `tui-shim-edit.desktop` | chooser for system/user copies; legacy "Open with" editor picker |
| `~/.local/share/icons/hicolor/{scalable,256x256}/apps/tui-shim-*` | downloaded or generated icons; cache in `~/.cache/tui-shim/icons` |
| `~/.config/tui-shim/choices.toml` | chosen copy per app; `recent.toml` recents; `view.toml` picker folder+scroll; `gap` inferred niri gap |
| `~/.cache/tui-shim/overlay.log` | diagnostic log |
| `$XDG_RUNTIME_DIR/tui-shim-<pid>.sock` / `.ctl` / `tui-shim-probe-*.reply` | wrapper input, control messages, probe answer (datagram sockets) |

Environment variables (all optional): `TUI_SHIM_PALETTE`, `TUI_SHIM_LOG`, `TUI_SHIM_DEBUG`, and for tests only
`TUI_SHIM_PROBE_TIMEOUT_MS`, `TUI_SHIM_PROBE_DELAY_MS`, `TUI_SHIM_MARKER_MS`, `TUI_SHIM_OVERLAY_EXPANDED`, `TUI_SHIM_DEMO_NAV`.
It reads `WAYLAND_DISPLAY`/`DISPLAY` (GUI available?), `NIRI_SOCKET` (overlay enabled?), `TERM_PROGRAM`/`TERM`
(terminal draws images?), `TERMINAL` (which terminal to spawn), `XDG_RUNTIME_DIR`, `HOME`.

Degradation: no display or `--tui` -> terminal pickers (crossterm); no `NIRI_SOCKET` -> no overlay, editors start normally;
terminal without kitty graphics -> the older layer-shell button instead of the in-terminal badge (position is then only
estimated, weaker); no `grim` -> panel centred when the layout is scrolled.

---------------------------------------------------------------------------------------------------

## 8. How to integrate it into Simple Linux / the ISO (recommendation)

1. **Package**: ebuild with `cargo.eclass`, `IUSE="+gui"`. Build deps and runtime deps as in section 5. Install the binary as
   `/usr/bin/tui-shim`. (The repo has no ebuild yet.)
2. **Binhost**: add `gui-libs/gtk4-layer-shell-1.1.1-r1` binary (see section 5) so the ISO does not need a compiler for it.
3. **Shims**: they have to come before `/usr/bin` in `PATH`. On the reference system `~/.local/bin` already does (fish). Ship
   `/etc/skel/.local/bin/micro` and `nvim` as symlinks to `/usr/bin/tui-shim`, or run `tui-shim shims install` once per user.
   `shims install` is hard-coded to `~/.local/bin`; if a system-wide directory is wanted, add a `--dir` option.
   Decide consciously about `$EDITOR`: an interactive `nvim` started by `git commit` also gets the badge.
4. **Launchers**: either run `tui-shim sync --no-theme` per user at first login (it writes `Exec=` with the path of the binary
   that ran it, so run the packaged `/usr/bin/tui-shim`), or ship static `.desktop` files in `/usr/share/applications`:
   `Exec=/usr/bin/tui-shim open micro %f`, `Terminal=false`, `Categories=Utility;TextEditor;`, plus the `X-TuiShim=true` marker.
5. **Palette**: ship the Simple Linux palette in the markdown format and make it the default (change the hard-coded path or set
   `TUI_SHIM_PALETTE`, e.g. via `environment.d`). Without a palette file the GUI uses plain libadwaita colours and the badge
   default colours.
6. **Terminal**: ghostty is the tested one. Any ghostty `window-padding-x/y` works; the badge sits in the grid corner and the
   padding is the margin to the window border.
7. **No autostart, no daemon, no systemd unit** are needed.
8. **Undo**: `tui-shim shims remove` (removes only symlinks that point at the binary), `tui-shim clean` (only marked launchers
   and `tui-shim-*` icons).

Open design points for the owner (Oleg) before the ISO: keep ghostty as the only supported terminal? ship the per-app theme
templates or not (see 6.12)? shims for more editors (helix, vim)? other compositors (the tracker is niri-specific, see 9)?

---------------------------------------------------------------------------------------------------

## 9. Known limitations and not verified

* **niri only** for tracking/focus/probe (`niri msg`). Hyprland/sway would need another backend in `track.rs`
  (`windows()`, `focused_window_id()`, event stream) and `probe::output_scale`.
* **kitty terminal** has never been run; only ghostty 1.1.3 was tested. Other terminals fall back to the layer-shell button.
* **Equal margins** badge-to-top and badge-to-right are approximate: the right side carries the grid's leftover sliver (up to one
  cell wide) which varies with the window width and is not reported. The circle sits ~0.3 row below the grid top to compensate
  on average.
* **Multi-monitor, rotated outputs, fractional scales other than 1.25** were not tested (the probe divides the screenshot by
  niri's scale; `grim` returns an upright image).
* **Editors run as root** (`doas nvim`) are not wrapped (the shims live in the user's `~/.local/bin`, and the picker needs
  the user's Wayland session).
* Only **micro** and **nvim** have `open_keys`; the user's LazyVim flicker on focus change is explained but not proven fixed.
* `tui-shim edit` (legacy editor chooser) and the 640x480 `pick` windows of the early design are still present and unused by
  the main flow; `README.md` predates the picker/badge work and does not describe it.
* No CI, no release process, version fixed at 0.1.0.

---------------------------------------------------------------------------------------------------

## 10. How it was tested (reproduce this, do not trust a screenshot alone)

* `cargo test` - 30 tests: input filter (clicks, typing, terminal replies), badge geometry incl. the scale-inflation case, probe
  blob finder with wallpaper-like noise, registry, files, palette, desktop quoting.
* **Wrapper under a pty** (python `pty`): send SGR mouse bytes and check the control socket (every cell of the badge registers, cells
  outside do not), send `a=d` and check the badge is re-uploaded, resize the pty and time the re-placement (~5 ms), probe round trip.
* **Nested niri**: `niri -c minimal.kdl -- script.sh` (a nested compositor in a window), editors started in nested `ghostty`
  windows, `grim` screenshots from inside, clicks simulated by sending `click` datagrams to the `.ctl` socket,
  `niri msg action focus-window/set-column-width` to change layout. The nested session is software-rendered and slow, so the
  `TUI_SHIM_PROBE_*`/`MARKER_MS` variables exist to lengthen waits there. It cannot inject real pointer input.
* Use an **isolated `HOME`** for tests (copy `~/.config/ghostty` into it): tests write `recent.toml`, `view.toml`, micro
  backups and the log into whatever `HOME` they run with. Never delete the real `recent.toml` wholesale.
* The owner tested interactively on the real session after each change; the screenshots he sent were the ground truth.

Quick smoke test on a fresh install: open a terminal, `micro` (picker opens by itself over micro with a round badge in the
corner), click the badge twice quickly (stays open), click it once more later (closes), pick a file (opens in the same window),
resize the window (badge follows), `nvim somefile` (badge, click opens the picker), `tui-shim shims status`.
