use crate::palette::Palette;
use crate::registry::App;
use anyhow::Result;
use std::path::{Path, PathBuf};

pub const MARKER: &str = "X-TuiShim=true";

pub fn apps_dir(home: &Path) -> PathBuf {
    home.join(".local/share/applications")
}

pub fn file_name(app: &App) -> String {
    format!("tui-shim-{}.desktop", app.name)
}

#[derive(Clone, Copy, PartialEq)]
pub enum Term {
    Ghostty,
    Alacritty,
    Foot,
    Kitty,
}

pub fn detect_terminal() -> Option<(Term, String)> {
    let mut cands: Vec<String> = Vec::new();
    if let Ok(t) = std::env::var("TERMINAL") {
        cands.push(t);
    }
    cands.extend(["ghostty", "alacritty", "foot", "kitty"].map(String::from));
    for c in cands {
        let base = Path::new(&c).file_name()?.to_string_lossy().into_owned();
        let term = match base.as_str() {
            "ghostty" => Term::Ghostty,
            "alacritty" => Term::Alacritty,
            "foot" => Term::Foot,
            "kitty" => Term::Kitty,
            _ => continue,
        };
        if crate::scan::which(&base).is_some() {
            return Some((term, base));
        }
    }
    None
}

/// Quote one argument per the Desktop Entry spec.
fn quote(a: &str) -> String {
    let a = a.replace('%', "%%");
    if a.chars().any(|c| " \t\n\"'\\><~|&;$*?#()`,=".contains(c)) {
        let esc = a.replace('\\', "\\\\").replace('"', "\\\"").replace('`', "\\`").replace('$', "\\$");
        format!("\"{esc}\"")
    } else {
        a
    }
}

pub fn exec_line(app: &App, bin: &Path, term: Term, term_bin: &str, pal: &Palette) -> String {
    let class = format!("tui-shim.{}", app.name);
    let mut args: Vec<String> = vec![term_bin.to_string()];
    let recolor = app.theme.fallback.as_deref() == Some("terminal");
    match term {
        Term::Ghostty => {
            args.push(format!("--class={class}"));
            args.push(format!("--title={}", app.title));
            if recolor {
                for (k, flag) in [("bg", "background"), ("fg", "foreground")] {
                    if let Some(c) = pal.get(k) {
                        args.push(format!("--{flag}={c}"));
                    }
                }
                let slots = ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"];
                for (i, s) in slots.iter().enumerate() {
                    if let Some(c) = pal.get(&format!("ansi_{s}")) {
                        args.push(format!("--palette={i}={c}"));
                    }
                    if let Some(c) = pal.get(&format!("ansi_{s}_bright")) {
                        args.push(format!("--palette={}={c}", i + 8));
                    }
                }
            }
            args.push("-e".into());
        }
        Term::Alacritty => {
            args.extend(["--class".into(), class, "-T".into(), app.title.clone(), "-e".into()]);
        }
        Term::Foot => {
            args.extend([format!("--app-id={class}"), format!("--title={}", app.title)]);
        }
        Term::Kitty => {
            args.extend(["--class".into(), class, "--title".into(), app.title.clone()]);
        }
    }
    if !app.theme.env.is_empty() {
        args.push("env".into());
        for (k, v) in &app.theme.env {
            args.push(format!("{k}={v}"));
        }
    }
    args.push(bin.to_string_lossy().into_owned());
    args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")
}

pub fn render(app: &App, exec: &str, icon: &str) -> String {
    let cats = app.categories.iter().map(|c| format!("{c};")).collect::<String>();
    let kws = app.keywords.iter().map(|c| format!("{c};")).collect::<String>();
    format!(
        "[Desktop Entry]\nType=Application\nName={}\nComment={}\nExec={exec}\nIcon={icon}\n\
         Terminal=false\nCategories={cats}\nKeywords={kws}\nStartupWMClass=tui-shim.{}\n{MARKER}\n",
        app.title, app.comment, app.name
    )
}

pub fn write(path: &Path, content: &str, dry: bool) -> Result<bool> {
    if std::fs::read_to_string(path).ok().as_deref() == Some(content) {
        return Ok(false);
    }
    if dry {
        println!("  would write {}", path.display());
    } else {
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(path, content)?;
        println!("  wrote {}", path.display());
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quoting() {
        assert_eq!(quote("btop"), "btop");
        assert_eq!(quote("A=b;c"), "\"A=b;c\"");
        assert_eq!(quote("100%"), "100%%");
    }
}
