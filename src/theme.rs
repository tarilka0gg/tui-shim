use crate::palette::Palette;
use crate::registry::{Activate, App, TEMPLATES};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

const BAK: &str = "bak-tui-shim";
const MANAGED: &str = "tui-shim managed";

pub fn expand(home: &Path, p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(p),
    }
}

fn backup(path: &Path, dry: bool) -> Result<()> {
    let bak = path.with_extension(match path.extension() {
        Some(e) => format!("{}.{BAK}", e.to_string_lossy()),
        None => BAK.to_string(),
    });
    if path.exists() && !bak.exists() && !dry {
        std::fs::copy(path, &bak)?;
        println!("  backup {}", bak.display());
    }
    Ok(())
}

/// Write the app's theme file and activate it. Returns true if anything changed.
pub fn apply(app: &App, home: &Path, pal: &Palette, dry: bool, force: bool) -> Result<bool> {
    let t = &app.theme;
    let (Some(path), Some(tpl)) = (&t.path, &t.template) else { return Ok(false) };
    let path = expand(home, path);
    let tpl = TEMPLATES.get_file(tpl).context("template missing")?.contents_utf8().unwrap();
    let body = pal.render(tpl)?;
    let mut changed = false;

    if let Ok(existing) = std::fs::read_to_string(&path) {
        if !existing.contains(MANAGED) && !force {
            println!("  skip {} (not managed by tui-shim; use --force)", path.display());
            return Ok(false);
        }
    }
    if std::fs::read_to_string(&path).ok().as_deref() != Some(&body) {
        changed = true;
        if dry {
            println!("  would write {}", path.display());
        } else {
            backup(&path, dry)?;
            std::fs::create_dir_all(path.parent().unwrap())?;
            std::fs::write(&path, &body)?;
            println!("  wrote {}", path.display());
        }
    }
    if let Some(act) = &t.activate {
        changed |= activate(act, home, dry)?;
    }
    Ok(changed)
}

fn activate(a: &Activate, home: &Path, dry: bool) -> Result<bool> {
    let file = expand(home, &a.file);
    let old = std::fs::read_to_string(&file).unwrap_or_default();
    let new = match a.style.as_str() {
        "quoted" => set_line(&old, &a.key, &format!("{} = \"{}\"", a.key, a.value)),
        "json" => {
            let mut v: serde_json::Value =
                serde_json::from_str(&old).unwrap_or(serde_json::json!({}));
            v[&a.key] = serde_json::Value::String(a.value.clone());
            serde_json::to_string_pretty(&v)? + "\n"
        }
        "yazi-flavor" => {
            let mut t: toml::Table = old.parse().unwrap_or_default();
            let flavor = t.entry("flavor").or_insert(toml::Value::Table(Default::default()));
            if let Some(ft) = flavor.as_table_mut() {
                ft.insert("dark".into(), toml::Value::String(a.value.clone()));
                ft.insert("light".into(), toml::Value::String(a.value.clone()));
            }
            toml::to_string_pretty(&t)?
        }
        s => anyhow::bail!("unknown activate style {s}"),
    };
    if new == old {
        return Ok(false);
    }
    if dry {
        println!("  would update {}", file.display());
    } else {
        backup(&file, dry)?;
        std::fs::create_dir_all(file.parent().unwrap())?;
        std::fs::write(&file, new)?;
        println!("  updated {}", file.display());
    }
    Ok(true)
}

/// Replace the line starting with `key` (`key=` or `key =`), or append.
fn set_line(text: &str, key: &str, line: &str) -> String {
    let mut found = false;
    let mut out: Vec<String> = text
        .lines()
        .map(|l| {
            let t = l.trim_start();
            if t.starts_with(key) && t[key.len()..].trim_start().starts_with('=') {
                found = true;
                line.to_string()
            } else {
                l.to_string()
            }
        })
        .collect();
    if !found {
        out.push(line.to_string());
    }
    out.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    use super::set_line;
    #[test]
    fn replaces_or_appends() {
        assert_eq!(set_line("a = 1\ncolor_theme = \"x\"\n", "color_theme", "color_theme = \"y\""), "a = 1\ncolor_theme = \"y\"\n");
        assert_eq!(set_line("a = 1\n", "k", "k = \"v\""), "a = 1\nk = \"v\"\n");
    }
}
