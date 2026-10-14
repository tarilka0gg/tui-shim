# Changelog

All notable changes to this project. Format follows [Keep a Changelog](https://keepachangelog.com/).

## [Unreleased]

### Added
- Picker search: case-insensitive, tiered ranking (exact, prefix, word start, substring, letters in order),
  several words, and recursive search below the current folder with a caption for hits in sub-folders.
- Gentoo live ebuild (`packaging/gentoo/`).
- Line-drawn fallback icons; `TUI_SHIM_LINE_ICONS` for line icons on every launcher.
- Default palette `~/.config/tui-shim/palette.md`, falling back to Futuro Darko.

### Changed
- Themed icons are referenced by name, not by file path.

### Fixed
- Palette unit test no longer collides on a shared temp file.

## [0.1.0] - 2026-10-14

### Added
- Registry-driven scanner for TUI apps (btop, htop, yazi, nvtop, micro, nvim, tmux, nmtui, lazygit, ncdu),
  launcher `.desktop` generation, icon lookup (installed themes, then Dashboard Icons / simple-icons, then a generated glyph)
  and per-app palette theming; `scan`, `sync`, `theme`, `clean`, `doctor`.
- Chooser between system and user-installed copies of a program.
- Editor file picker: GTK4/libadwaita grid with a Thunar-style bar, animations, recents, remembered folder and scroll.
- Pty wrapper for micro and nvim that draws a folder badge in the terminal (kitty graphics), handles click/F9,
  places the picker next to the badge (niri IPC, layout estimate or screenshot probe) and opens the picked file in the same editor.
- `shims install` to make plain `micro`/`nvim` use the wrapper; auto-open of the picker for editors started without a file.
- Integration handoff document (`docs/INTEGRATION.md`).
