# Copyright 2026 Gentoo Authors
# Distributed under the terms of the GNU General Public License v2

EAPI=8

CRATES=""

RUST_MIN_VER="1.85"

inherit cargo git-r3

DESCRIPTION="Makes terminal programs behave like desktop apps: launchers, icons and a file picker for editors"
HOMEPAGE="https://github.com/tarilka0gg/tui-shim"
EGIT_REPO_URI="https://github.com/tarilka0gg/tui-shim.git"
# to build a local checkout: EGIT_OVERRIDE_REPO_TARILKA0GG_TUI_SHIM=file:///path/to/tui-shim

LICENSE="GPL-2+"
LICENSE+=" Apache-2.0 BSD ISC MIT Unicode-3.0 ZLIB"
SLOT="0"
IUSE="+gui"

# gui: the file picker (GTK4 + libadwaita on a layer-shell surface). grim finds the badge when niri's columns are scrolled.
RDEPEND="
	gui? (
		>=gui-libs/gtk-4.10:4
		>=gui-libs/libadwaita-1.5:1
		gui-libs/gtk4-layer-shell
		gui-apps/grim
	)
"
DEPEND="
	gui? (
		>=gui-libs/gtk-4.10:4
		>=gui-libs/libadwaita-1.5:1
		gui-libs/gtk4-layer-shell
	)
"
BDEPEND="virtual/pkgconfig"

src_unpack() {
	git-r3_src_unpack
	cargo_live_src_unpack
}

pkg_setup() {
	rust_pkg_setup
}

src_configure() {
	local myfeatures=( $(usev gui) )
	cargo_src_configure --no-default-features
}

src_compile() {
	cargo_src_compile
}

src_install() {
	cargo_src_install
	einstalldocs
}

pkg_postinst() {
	elog "Run 'tui-shim shims install' as your user to put micro and nvim under the file-picker badge,"
	elog "and 'tui-shim sync --no-theme' to write the launchers (no theme files are touched)."
}
