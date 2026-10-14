use crate::palette::Palette;
use crate::registry::App;
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Tier 1: look through installed icon themes. Returns an absolute path.
pub fn find_installed(home: &Path, names: &[String]) -> Option<PathBuf> {
    let roots = [
        home.join(".local/share/icons"),
        PathBuf::from("/usr/share/icons"),
        PathBuf::from("/usr/share/pixmaps"),
    ];
    let mut best: Option<(i32, PathBuf)> = None;
    for root in roots {
        walk(&root, 0, &mut |p| {
            let (Some(stem), Some(ext)) = (p.file_stem(), p.extension()) else { return };
            let stem = stem.to_string_lossy();
            if !names.iter().any(|n| *n == stem) || !matches!(ext.to_str(), Some("svg" | "png")) {
                return;
            }
            let s = p.to_string_lossy();
            if s.contains("/symbolic") || s.ends_with("-symbolic.svg") {
                return;
            }
            let mut score = if ext == "svg" { 1000 } else { 0 };
            score += s.split('/').find_map(|seg| seg.split('x').next()?.parse::<i32>().ok()).unwrap_or(0);
            // prefer earlier names in the registry list
            score += 500 * (names.len() - names.iter().position(|n| *n == stem).unwrap_or(0)) as i32;
            if best.as_ref().is_none_or(|(b, _)| score > *b) {
                best = Some((score, p.to_path_buf()));
            }
        });
    }
    best.map(|(_, p)| p)
}

fn walk(dir: &Path, depth: u32, f: &mut dyn FnMut(&Path)) {
    if depth > 6 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, depth + 1, f);
        } else if p.to_string_lossy().contains("/apps/") || depth == 0 {
            f(&p);
        }
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(8)))
        .build()
        .into()
}

fn fetch(url: &str, want_svg: bool) -> Result<Vec<u8>> {
    let mut resp = agent().get(url).call()?;
    let body = resp.body_mut().with_config().limit(2 << 20).read_to_vec()?;
    let ok = if want_svg {
        std::str::from_utf8(&body).is_ok_and(|s| s.contains("<svg"))
    } else {
        body.starts_with(b"\x89PNG")
    };
    if !ok {
        bail!("unexpected content from {url}");
    }
    Ok(body)
}

/// Tier 2: web. Returns (bytes, is_svg).
pub fn download(app: &App) -> Option<(Vec<u8>, bool)> {
    let mut urls: Vec<(String, bool)> = Vec::new();
    if let Some(d) = &app.dashboard_icon {
        urls.push((format!("https://cdn.jsdelivr.net/gh/homarr-labs/dashboard-icons/svg/{d}.svg"), true));
        urls.push((format!("https://cdn.jsdelivr.net/gh/homarr-labs/dashboard-icons/png/{d}.png"), false));
    }
    if let Some(s) = &app.simple_icon {
        urls.push((format!("https://cdn.jsdelivr.net/npm/simple-icons/icons/{s}.svg"), true));
    }
    for (u, svg) in urls {
        match fetch(&u, svg) {
            Ok(b) => return Some((b, svg)),
            Err(e) => eprintln!("    icon source failed: {e}"),
        }
    }
    None
}

/// Marks an icon this function made, so a hook may replace it with a fresh one when the palette changes.
pub const GLYPH_MARK: &str = "<!--tui-shim-glyph-->";

/// Last resort, so an entry is never blank: a line drawing of a terminal window with a picture of the program inside. Strokes only, no
/// filled areas and no text (launchers that tint icons would turn a filled square into a blob, and text needs a font).
pub fn glyph(app: &App, pal: &Palette) -> String {
    let fg = pal.get("fg").unwrap_or("#8A8A8A");
    let ac = pal.get("accent").unwrap_or("#BD5636");
    let line = |d: &str, c: &str| format!("<path d=\"{d}\" fill=\"none\" stroke=\"{c}\" stroke-width=\"12\" stroke-linecap=\"round\" stroke-linejoin=\"round\"/>");
    let inner = match app.name.as_str() {
        "btop" | "htop" | "nvtop" => [line("M84 196V152", ac), line("M116 196V112", fg), line("M148 196V132", ac), line("M180 196V92", fg)].concat(),
        "tmux" => [line("M128 76V196", fg), line("M128 136H196", ac)].concat(),
        _ if app.categories.iter().any(|c| c == "TextEditor") => {
            [line("M72 100H184", fg), line("M72 136H160", fg), line("M72 172H120", fg), line("M156 164V184", ac)].concat()
        }
        _ => [line("M80 104L116 136L80 168", ac), line("M132 172H184", fg)].concat(),
    };
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 256 256\">{GLYPH_MARK}\
         <rect x=\"28\" y=\"36\" width=\"200\" height=\"184\" rx=\"28\" fill=\"none\" stroke=\"{fg}\" stroke-width=\"12\"/>\
         {}</svg>",
        [line("M28 72H228", fg), inner].concat()
    )
}

/// Resolve the Icon= value for an app, installing/caching as needed.
pub fn resolve(app: &App, home: &Path, pal: &Palette, dry: bool, offline: bool) -> Result<String> {
    if let Some(p) = find_installed(home, &app.icon_names) {
        return Ok(p.to_string_lossy().into_owned());
    }
    let id = format!("tui-shim-{}", app.name);
    let svg_dst = home.join(format!(".local/share/icons/hicolor/scalable/apps/{id}.svg"));
    let png_dst = home.join(format!(".local/share/icons/hicolor/256x256/apps/{id}.png"));
    if svg_dst.exists() || png_dst.exists() {
        return Ok(id);
    }
    let cache = home.join(".cache/tui-shim/icons");
    let (bytes, svg) = if offline {
        (glyph(app, pal).into_bytes(), true)
    } else {
        download(app).unwrap_or_else(|| (glyph(app, pal).into_bytes(), true))
    };
    let dst = if svg { svg_dst } else { png_dst };
    if dry {
        println!("  would install icon {}", dst.display());
    } else {
        std::fs::create_dir_all(&cache)?;
        std::fs::write(cache.join(format!("{}.{}", app.name, if svg { "svg" } else { "png" })), &bytes)?;
        std::fs::create_dir_all(dst.parent().unwrap())?;
        std::fs::write(&dst, &bytes)?;
        println!("  installed icon {}", dst.display());
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn app(name: &str, dash: Option<&str>, gh: Option<&str>) -> App {
        App {
            name: name.into(), title: name.into(), comment: String::new(),
            categories: vec![], keywords: vec![], icon_names: vec![],
            dashboard_icon: dash.map(Into::into), simple_icon: gh.map(Into::into), version_arg: None, builtin_picker: false, open_keys: None, open_quote: None,
            theme: Default::default(),
        }
    }
    #[test]
    fn glyph_is_svg() {
        let pal = Palette { name: "t".into(), colors: Default::default() };
        let g = glyph(&app("zzz", None, None), &pal);
        assert!(g.starts_with("<svg") && g.contains(GLYPH_MARK) && !g.contains("<text"));
    }
    #[test]
    #[ignore = "writes /tmp/glyph-*.svg for a look"]
    fn dump_glyphs() {
        let pal = Palette { name: "t".into(), colors: [("fg".to_string(), "#E0E0E0".to_string()), ("accent".to_string(), "#D0605A".to_string())].into() };
        for (n, c) in [("btop", "Utility"), ("tmux", "Utility"), ("micro", "TextEditor"), ("yazi", "Utility")] {
            let mut a = app(n, None, None);
            a.categories = vec![c.into()];
            std::fs::write(format!("/tmp/glyph-{n}.svg"), glyph(&a, &pal)).unwrap();
        }
    }
    #[test]
    #[ignore = "needs network"]
    fn web_tier() {
        let (b, svg) = download(&app("nvim", Some("neovim"), Some("neovim"))).expect("download");
        assert!(svg && b.len() > 100);
    }
}
