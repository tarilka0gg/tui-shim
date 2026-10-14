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

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Up,
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

/// What the picker shows for `dir` and the text typed so far.
pub fn candidates(home: &Path, dir: &Path, query: &str) -> Vec<Entry> {
    let mut out = Vec::new();
    let q = query.trim();
    if let Some(parent) = dir.parent() {
        if q.is_empty() {
            out.push(Entry { path: parent.to_path_buf(), label: "../".into(), detail: "up".into(), kind: Kind::Up });
        }
    }
    if q.is_empty() {
        for r in recent(home).into_iter().take(6) {
            let label = r.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let detail = r.parent().map(|p| p.display().to_string()).unwrap_or_default();
            out.push(Entry { path: r, label, detail, kind: Kind::Recent });
        }
        out.extend(list_dir(dir, false));
        return out;
    }
    let mut scored: Vec<(i32, Entry)> = list_dir(dir, q.starts_with('.'))
        .into_iter()
        .filter_map(|e| fuzzy(q, &e.label).map(|s| (s, e)))
        .collect();
    for r in recent(home) {
        let label = r.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if let Some(s) = fuzzy(q, &label) {
            let detail = r.parent().map(|p| p.display().to_string()).unwrap_or_default();
            scored.push((s + 5, Entry { path: r, label, detail, kind: Kind::Recent }));
        }
    }
    scored.sort_by_key(|(s, _)| -s);
    out.extend(scored.into_iter().map(|(_, e)| e));
    let new_path = if q.starts_with('/') { PathBuf::from(q) } else if let Some(r) = q.strip_prefix("~/") { home.join(r) } else { dir.join(q) };
    out.push(Entry { label: format!("New file: {q}"), detail: new_path.display().to_string(), path: new_path, kind: Kind::New });
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
        let c = candidates(&d, &d, "alp");
        assert_eq!(c[0].label, "alpha.txt");
        assert_eq!(c.last().unwrap().kind, Kind::New);
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
