use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Persistent user decisions, kept apart from generated files so `sync` never overrides them.
#[derive(Default, Serialize, Deserialize)]
pub struct Choices {
    /// app name -> chosen binary path
    #[serde(default)]
    pub bin: BTreeMap<String, String>,
    /// last editor picked in `edit`
    pub editor: Option<String>,
}

fn path(home: &Path) -> PathBuf {
    home.join(".config/tui-shim/choices.toml")
}

impl Choices {
    pub fn load(home: &Path) -> Self {
        std::fs::read_to_string(path(home))
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, home: &Path) -> Result<()> {
        let p = path(home);
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(p, toml::to_string_pretty(self)?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip() {
        let home = std::env::temp_dir().join("tui-shim-choices-test");
        let _ = std::fs::remove_dir_all(&home);
        let mut c = Choices::default();
        c.bin.insert("yazi".into(), "/home/x/.cargo/bin/yazi".into());
        c.editor = Some("micro".into());
        c.save(&home).unwrap();
        let l = Choices::load(&home);
        assert_eq!(l.bin["yazi"], "/home/x/.cargo/bin/yazi");
        assert_eq!(l.editor.as_deref(), Some("micro"));
    }
}
