use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Palette {
    pub name: String,
    pub colors: BTreeMap<String, String>,
}

impl Palette {
    /// Parse the markdown palette tables (`| `name` | `#HEX` | ...`).
    /// Rows under "## ANSI" become `ansi_<slot>` / `ansi_<slot>_bright`.
    pub fn from_markdown(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading palette {}", path.display()))?;
        let mut colors = BTreeMap::new();
        let mut ansi = false;
        for line in text.lines() {
            if line.starts_with("## ") {
                ansi = line.contains("ANSI");
            }
            if !line.starts_with('|') {
                continue;
            }
            let cells: Vec<String> = line
                .trim_matches('|')
                .split('|')
                .map(|c| c.trim().trim_matches('`').to_string())
                .collect();
            if cells.len() < 2 || !is_hex(&cells[1]) {
                continue;
            }
            let key = cells[0].to_string();
            if ansi {
                colors.insert(format!("ansi_{key}"), cells[1].clone());
                if let Some(b) = cells.get(2).filter(|c| is_hex(c)) {
                    colors.insert(format!("ansi_{key}_bright"), b.clone());
                }
            } else {
                colors.insert(key, cells[1].clone());
            }
        }
        // derived tones listed in prose, not tables
        for line in text.lines() {
            if let Some(rest) = line.trim().strip_prefix("- `") {
                if let Some((k, tail)) = rest.split_once('`') {
                    if let Some(hex) = tail.split('`').nth(1).filter(|h| is_hex(h)) {
                        colors.entry(k.to_string()).or_insert(hex.to_string());
                    }
                }
            }
        }
        for need in ["bg", "fg", "accent"] {
            if !colors.contains_key(need) {
                bail!("palette {} lacks required color `{need}`", path.display());
            }
        }
        Ok(Self { name: "system".into(), colors })
    }

    pub fn get(&self, k: &str) -> Option<&str> {
        self.colors.get(k).map(String::as_str)
    }

    /// Replace `{{key}}` placeholders; unknown keys are an error.
    pub fn render(&self, tpl: &str) -> Result<String> {
        let mut out = String::new();
        let mut rest = tpl;
        while let Some(i) = rest.find("{{") {
            out.push_str(&rest[..i]);
            let j = rest[i..].find("}}").context("unclosed {{")? + i;
            let key = rest[i + 2..j].trim();
            if key == "palette_name" {
                out.push_str(&self.name);
            } else {
                out.push_str(self.get(key).with_context(|| format!("unknown palette key `{key}`"))?);
            }
            rest = &rest[j + 2..];
        }
        out.push_str(rest);
        Ok(out)
    }
}

fn is_hex(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_and_renders() {
        let dir = std::env::temp_dir().join(format!("tui-shim-pal-test-{}.md", std::process::id()));
        std::fs::write(&dir, "| `bg` | `#BCC6B7` | x |\n| `fg` | `#3B342B` | x |\n| `accent` | `#BD5636` | x |\n## ANSI\n| red | `#BD5636` | `#BD5636` |\n").unwrap();
        let p = Palette::from_markdown(&dir).unwrap();
        assert_eq!(p.render("{{bg}}-{{ansi_red_bright}}").unwrap(), "#BCC6B7-#BD5636");
        assert!(p.render("{{nope}}").is_err());
    }
}
