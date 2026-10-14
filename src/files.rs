//! Data side of the file picker: recent files, directory listing, fuzzy filtering.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const MAX_RECENT: usize = 30;

#[derive(Default, Serialize, Deserialize)]
struct RecentFile {
    #[serde(default)]
    files: Vec<String>,
}

fn recent_path(home: &Path) -> PathBuf {
    home.join(".config/tui-shim/recent.toml")
}

pub fn recent(home: &Path) -> Vec<PathBuf> {
    let r: RecentFile = std::fs::read_to_string(recent_path(home))
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default();
    r.files.into_iter().map(PathBuf::from).filter(|p| p.is_file()).collect()
}

pub fn push_recent(home: &Path, file: &Path) {
    let mut files: Vec<String> = recent(home).iter().map(|p| p.to_string_lossy().into_owned()).collect();
    let f = file.to_string_lossy().into_owned();
    files.retain(|x| *x != f);
    files.insert(0, f);
    files.truncate(MAX_RECENT);
    let p = recent_path(home);
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    if let Ok(s) = toml::to_string_pretty(&RecentFile { files }) {
        let _ = std::fs::write(p, s);
    }
}

/// Where the user left off in the picker: kept on disk so a new editor window resumes there.
#[derive(Clone, Default, Debug)]
pub struct View {
    pub dir: Option<PathBuf>,
    pub scroll: Option<f64>,
}

#[derive(Default, Serialize, Deserialize)]
struct ViewFile {
    dir: Option<String>,
    scroll: Option<f64>,
}

fn view_path(home: &Path) -> PathBuf {
    home.join(".config/tui-shim/view.toml")
}

pub fn load_view(home: &Path) -> View {
    let v: ViewFile = std::fs::read_to_string(view_path(home)).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default();
    View { dir: v.dir.map(PathBuf::from).filter(|d| d.is_dir()), scroll: v.scroll }
}

pub fn save_view(home: &Path, v: &View) {
    let p = view_path(home);
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    let f = ViewFile { dir: v.dir.as_ref().map(|d| d.to_string_lossy().into_owned()), scroll: v.scroll };
    if let Ok(s) = toml::to_string_pretty(&f) {
        let _ = std::fs::write(p, s);
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Dir,
    File,
    Recent,
    New,
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub path: PathBuf,
    pub label: String,
    pub detail: String,
    pub kind: Kind,
}

/// Subsequence match: every query char appears in order. Higher is better; None = no match.
pub fn fuzzy(query: &str, text: &str) -> Option<i32> {
    let (q, t): (Vec<char>, Vec<char>) = (query.to_lowercase().chars().collect(), text.to_lowercase().chars().collect());
    let mut score = 0;
    let mut ti = 0;
    let mut prev: Option<usize> = None;
    for qc in &q {
        let pos = t[ti..].iter().position(|c| c == qc)? + ti;
        score += 10;
        if prev.is_some_and(|p| p + 1 == pos) {
            score += 15; // consecutive
        }
        if pos == 0 || matches!(t[pos - 1], '/' | '.' | '_' | '-' | ' ') {
            score += 10; // word start
        }
        prev = Some(pos);
        ti = pos + 1;
    }
    Some(score - (t.len() as i32 - q.len() as i32).min(30))
}

pub fn list_dir(dir: &Path, show_hidden: bool) -> Vec<Entry> {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !show_hidden && name.starts_with('.') {
                continue;
            }
            let path = e.path();
            if path.is_dir() {
                dirs.push(Entry { label: format!("{name}/"), detail: String::new(), path, kind: Kind::Dir });
            } else {
                let size = e.metadata().map(|m| human(m.len())).unwrap_or_default();
                files.push(Entry { label: name, detail: size, path, kind: Kind::File });
            }
        }
    }
    let key = |e: &Entry| e.label.to_lowercase();
    dirs.sort_by_key(key);
    files.sort_by_key(key);
    dirs.into_iter().chain(files).collect()
}

fn human(n: u64) -> String {
    match n {
        0..=1023 => format!("{n} B"),
        1024..=1048575 => format!("{:.1} KB", n as f64 / 1024.0),
        _ => format!("{:.1} MB", n as f64 / 1048576.0),
    }
}

/// Tiles for the grid: the directory (or recent files), filtered by `query`.
/// A non-empty query also offers "create this file".
pub fn view(home: &Path, dir: &Path, show_hidden: bool, recent_mode: bool, query: &str) -> Vec<Entry> {
    let q = query.trim();
    let base: Vec<Entry> = if recent_mode {
        recent(home)
            .into_iter()
            .map(|r| {
                let label = r.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let detail = r.parent().map(|p| p.display().to_string()).unwrap_or_default();
                Entry { path: r, label, detail, kind: Kind::Recent }
            })
            .collect()
    } else {
        list_dir(dir, show_hidden || q.starts_with('.'))
    };
    if q.is_empty() {
        return base;
    }
    let mut scored: Vec<(i32, Entry)> = base.into_iter().filter_map(|e| fuzzy(q, &e.label).map(|s| (s, e))).collect();
    scored.sort_by_key(|(s, _)| -s);
    let mut out: Vec<Entry> = scored.into_iter().map(|(_, e)| e).collect();
    let new_path = if q.starts_with('/') {
        PathBuf::from(q)
    } else if let Some(r) = q.strip_prefix("~/") {
        home.join(r)
    } else {
        dir.join(q)
    };
    out.push(Entry { label: q.to_string(), detail: new_path.display().to_string(), path: new_path, kind: Kind::New });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_basics() {
        assert!(fuzzy("mn", "main.rs").is_some());
        assert!(fuzzy("xyz", "main.rs").is_none());
        assert!(fuzzy("main", "main.rs") > fuzzy("main", "a_long_dir_name_with_main_inside.rs"));
    }

    #[test]
    fn lists_and_filters() {
        let d = std::env::temp_dir().join("tui-shim-files-test");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("alpha.txt"), "x").unwrap();
        std::fs::write(d.join(".hidden"), "x").unwrap();
        let l = list_dir(&d, false);
        assert_eq!(l.iter().map(|e| e.label.as_str()).collect::<Vec<_>>(), ["sub/", "alpha.txt"]);
        let c = view(&d, &d, false, false, "alp");
        assert_eq!(c[0].label, "alpha.txt");
        assert_eq!(c.last().unwrap().kind, Kind::New);
        assert_eq!(view(&d, &d, false, false, "").len(), 2);
    }

    #[test]
    fn view_roundtrip() {
        let home = std::env::temp_dir().join("tui-shim-view-test");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("sub")).unwrap();
        save_view(&home, &View { dir: Some(home.join("sub")), scroll: Some(123.5) });
        let v = load_view(&home);
        assert_eq!(v.dir, Some(home.join("sub")));
        assert_eq!(v.scroll, Some(123.5));
        std::fs::remove_dir_all(home.join("sub")).unwrap();
        assert!(load_view(&home).dir.is_none(), "a folder that no longer exists is forgotten");
    }

    #[test]
    fn recent_roundtrip() {
        let home = std::env::temp_dir().join("tui-shim-recent-test");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let f = home.join("a.txt");
        std::fs::write(&f, "x").unwrap();
        push_recent(&home, &f);
        assert_eq!(recent(&home), vec![f]);
    }
}
