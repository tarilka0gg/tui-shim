use anyhow::{Context, Result};
use include_dir::{include_dir, Dir};
use serde::Deserialize;
use std::collections::BTreeMap;

static REGISTRY: Dir = include_dir!("$CARGO_MANIFEST_DIR/registry");
pub static TEMPLATES: Dir = include_dir!("$CARGO_MANIFEST_DIR/templates");

#[derive(Debug, Deserialize, Clone)]
pub struct App {
    pub name: String,
    pub title: String,
    #[serde(default)]
    pub comment: String,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub icon_names: Vec<String>,
    pub dashboard_icon: Option<String>,
    pub simple_icon: Option<String>,
    /// flag that prints the version (default --version)
    pub version_arg: Option<String>,
    /// editor already has its own file picker (nvim): no picker before launch
    #[serde(default)]
    pub builtin_picker: bool,
    /// bytes typed into the running editor to open a file; {file} is substituted
    pub open_keys: Option<String>,
    /// how {file} is quoted for the editor: "shell" (double quotes) or "vim" (backslashes)
    pub open_quote: Option<String>,
    #[serde(default)]
    pub theme: Theme,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct Theme {
    pub path: Option<String>,
    pub template: Option<String>,
    pub activate: Option<Activate>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// "terminal": recolor via terminal-level ANSI override in the launcher.
    pub fallback: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Activate {
    pub file: String,
    pub key: String,
    pub value: String,
    /// quoted | json | yazi-flavor
    pub style: String,
}

pub fn load() -> Result<Vec<App>> {
    let mut apps = Vec::new();
    for f in REGISTRY.files() {
        let text = f.contents_utf8().context("registry file not utf8")?;
        let app: App = toml::from_str(text)
            .with_context(|| format!("parsing registry/{}", f.path().display()))?;
        apps.push(app);
    }
    apps.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(apps)
}

#[cfg(test)]
mod tests {
    #[test]
    fn registry_parses() {
        let apps = super::load().unwrap();
        assert!(apps.iter().any(|a| a.name == "btop"));
        for a in &apps {
            if let Some(p) = &a.theme.template {
                assert!(super::TEMPLATES.get_file(p).is_some(), "missing template {p}");
            }
        }
    }
}

impl App {
    /// Keystrokes that make the running editor open `file`, if we know how.
    pub fn open_keys(&self, file: &std::path::Path) -> Option<Vec<u8>> {
        let tpl = self.open_keys.as_deref()?;
        let f = file.to_string_lossy();
        let quoted = match self.open_quote.as_deref() {
            Some("vim") => f.chars().flat_map(|c| if " \\|%#\"'".contains(c) { vec!['\\', c] } else { vec![c] }).collect::<String>(),
            _ => format!("\"{}\"", f.replace('\\', "\\\\").replace('"', "\\\"")),
        };
        Some(tpl.replace("{file}", &quoted).into_bytes())
    }
}

#[cfg(test)]
mod key_tests {
    #[test]
    fn open_keys_quote() {
        let apps = super::load().unwrap();
        let micro = apps.iter().find(|a| a.name == "micro").unwrap();
        assert_eq!(micro.open_keys(std::path::Path::new("/a b/c.txt")).unwrap(), b"\x05open \"/a b/c.txt\"\r");
        let nvim = apps.iter().find(|a| a.name == "nvim").unwrap();
        assert_eq!(nvim.open_keys(std::path::Path::new("/a b/c.txt")).unwrap(), b"\x1b:e /a\\ b/c.txt\r");
    }
}
