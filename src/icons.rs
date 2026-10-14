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

/// Last resort: palette-coloured letter glyph so an entry is never blank.
pub fn glyph(app: &App, pal: &Palette) -> String {
    let bg = pal.get("bg_alt").unwrap_or("#DAD4C1");
    let fg = pal.get("fg").unwrap_or("#3B342B");
    let ac = pal.get("accent").unwrap_or("#BD5636");
    let ch = app.title.chars().next().unwrap_or('?').to_uppercase();
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 256 256\">\
         <rect width=\"256\" height=\"256\" rx=\"48\" fill=\"{bg}\"/>\
         <rect x=\"16\" y=\"16\" width=\"224\" height=\"224\" rx=\"36\" fill=\"none\" stroke=\"{ac}\" stroke-width=\"8\"/>\
         <text x=\"128\" y=\"176\" font-family=\"monospace\" font-weight=\"bold\" font-size=\"140\" \
         text-anchor=\"middle\" fill=\"{fg}\">{ch}</text></svg>"
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
        assert!(g.starts_with("<svg") && g.contains(">Z<"));
    }
    #[test]
    #[ignore = "needs network"]
    fn web_tier() {
        let (b, svg) = download(&app("nvim", Some("neovim"), Some("neovim"))).expect("download");
        assert!(svg && b.len() > 100);
    }
}
