//! Data side of the file picker: recent files, directory listing, fuzzy filtering.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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
    /// folder of a search hit, relative to where the search started ("" for the folder itself)
    pub parent: String,
}

/// Lower-case one character the same way everywhere (Unicode aware, one char in, one char out, so
/// positions stay aligned with the original text: "DEsktop" and "десктоп" fold the way people expect).
fn lc(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Subsequence match: every query char appears in order. Higher is better; None = no match.
/// Case does not matter.
pub fn fuzzy(query: &str, text: &str) -> Option<i32> {
    let (q, t): (Vec<char>, Vec<char>) = (query.chars().map(lc).collect(), text.chars().map(lc).collect());
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

/// How well one word matches a name, in tiers so that the kind of match decides the order first:
/// exact (1000) > prefix (800) > start of a word inside the name (600) > anywhere inside (400) >
/// letters in order (100..199). Case-insensitive. None = does not match at all.
pub fn rank(word: &str, name: &str) -> Option<i32> {
    let w: Vec<char> = word.chars().map(lc).collect();
    if w.is_empty() {
        return Some(0);
    }
    let orig: Vec<char> = name.trim_end_matches('/').chars().collect();
    let t: Vec<char> = orig.iter().copied().map(lc).collect();
    let extra = (t.len() as i32 - w.len() as i32).clamp(0, 60); // a shorter name is a closer match
    if t == w {
        return Some(1000);
    }
    if t.starts_with(&w) {
        return Some(800 - extra);
    }
    let boundary = |i: usize| i == 0 || !orig[i - 1].is_alphanumeric() || (orig[i - 1].is_lowercase() && orig[i].is_uppercase());
    let mut first_inside = None;
    for i in 0..=t.len().saturating_sub(w.len()) {
        if t[i..].starts_with(&w) {
            if boundary(i) {
                return Some(600 - extra - (i as i32).min(50));
            }
            first_inside.get_or_insert(i);
        }
    }
    if let Some(i) = first_inside {
        return Some(400 - extra - (i as i32).min(50));
    }
    fuzzy(word, name.trim_end_matches('/')).map(|s| 100 + s.clamp(0, 99))
}

/// Several words: all of them must match (the name, or else the folder it is in), scores add up.
fn rank_words(words: &[String], name: &str, parent_lower: &str) -> Option<i32> {
    let mut total = 0;
    for w in words {
        let s = match rank(w, name) {
            Some(s) => s,
            None if !parent_lower.is_empty() && parent_lower.contains(w.as_str()) => 150, // "scripts babel"
            None => return None,
        };
        total += s;
    }
    Some(total)
}

fn words_of(query: &str) -> Vec<String> {
    query.split_whitespace().map(|w| w.chars().map(lc).collect()).collect()
}

fn new_entry(q: &str, new_path: PathBuf) -> Entry {
    Entry { label: q.to_string(), detail: new_path.display().to_string(), path: new_path, kind: Kind::New, parent: String::new() }
}

/// Folders never worth walking into when searching.
const SKIP_DIRS: &[&str] = &["node_modules", "target", "__pycache__", "site-packages", ".git", ".cache", ".cargo", ".rustup", ".npm", ".local/share/Trash"];
const MAX_DEPTH: usize = 6;
const MAX_SCANNED: usize = 120_000;
const MAX_RESULTS: usize = 160;

/// Search `root` and everything below it (breadth first, so near results are found first even if the
/// walk is cut short). Case-insensitive, several words allowed, best matches first.
pub fn search(root: &Path, query: &str, show_hidden: bool, budget: Duration) -> Vec<Entry> {
    use std::collections::VecDeque;
    let words = words_of(query);
    if words.is_empty() {
        return Vec::new();
    }
    let started = Instant::now();
    let show_hidden = show_hidden || query.trim_start().starts_with('.');
    let mut queue: VecDeque<(PathBuf, usize)> = VecDeque::from([(root.to_path_buf(), 0)]);
    let mut hits: Vec<(i32, usize, Entry)> = Vec::new();
    let mut scanned = 0usize;
    'walk: while let Some((dir, depth)) = queue.pop_front() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let rel_parent = dir.strip_prefix(root).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        let parent_lower: String = rel_parent.chars().map(lc).collect();
        for e in rd.flatten() {
            scanned += 1;
            if scanned > MAX_SCANNED || (scanned % 512 == 0 && started.elapsed() > budget) {
                break 'walk;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            if !show_hidden && name.starts_with('.') {
                continue;
            }
            let Ok(ft) = e.file_type() else { continue };
            let path = e.path();
            let is_dir = ft.is_dir() || (ft.is_symlink() && path.is_dir());
            if let Some(score) = rank_words(&words, &name, &parent_lower) {
                let label = if is_dir { format!("{name}/") } else { name.clone() };
                let rel = if rel_parent.is_empty() { name.clone() } else { format!("{rel_parent}/{name}") };
                let entry = Entry { path: path.clone(), label, detail: rel, kind: if is_dir { Kind::Dir } else { Kind::File }, parent: rel_parent.clone() };
                hits.push((score - 6 * depth as i32, depth, entry));
            }
            if ft.is_dir() && depth < MAX_DEPTH && !SKIP_DIRS.contains(&name.as_str()) {
                queue.push_back((path, depth + 1));
            }
        }
    }
    hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.label.len().cmp(&b.2.label.len())).then_with(|| a.2.label.to_lowercase().cmp(&b.2.label.to_lowercase())));
    hits.truncate(MAX_RESULTS);
    hits.into_iter()
        .map(|(_, _, mut e)| {
            if e.kind == Kind::File {
                if let Ok(m) = std::fs::metadata(&e.path) {
                    e.detail = format!("{}  -  {}", e.detail, human(m.len()));
                }
            }
            e
        })
        .collect()
}

/// The tile for "create the file named like the query".
pub fn new_file_entry(home: &Path, dir: &Path, query: &str) -> Entry {
    let q = query.trim();
    let new_path = if q.starts_with('/') {
        PathBuf::from(q)
    } else if let Some(r) = q.strip_prefix("~/") {
        home.join(r)
    } else {
        dir.join(q)
    };
    new_entry(q, new_path)
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
                dirs.push(Entry { label: format!("{name}/"), detail: String::new(), path, kind: Kind::Dir, parent: String::new() });
            } else {
                let size = e.metadata().map(|m| human(m.len())).unwrap_or_default();
                files.push(Entry { label: name, detail: size, path, kind: Kind::File, parent: String::new() });
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
                Entry { path: r, label, detail, kind: Kind::Recent, parent: String::new() }
            })
            .collect()
    } else {
        list_dir(dir, show_hidden || q.starts_with('.'))
    };
    if q.is_empty() {
        return base;
    }
    let words = words_of(q);
    let mut scored: Vec<(i32, Entry)> = base.into_iter().filter_map(|e| rank_words(&words, &e.label, "").map(|s| (s, e))).collect();
    scored.sort_by_key(|(s, _)| -s);
    let mut out: Vec<Entry> = scored.into_iter().map(|(_, e)| e).collect();
    out.push(new_file_entry(home, dir, q));
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

#[cfg(test)]
mod search_tests {
    use super::*;

    fn tree(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tui-shim-search-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for dir in ["Desktop", "desktop", "DeSKtop", "DESKTOP-old", "Downloads", "dev-skills-tool", "notes_desktop_backup", "desktop/scripts", "node_modules/desktop"] {
            std::fs::create_dir_all(d.join(dir)).unwrap();
        }
        std::fs::write(d.join("desktop/scripts/babel.config.json"), "{}").unwrap();
        std::fs::write(d.join(".hidden_desktop"), "").unwrap();
        d
    }

    fn names(v: &[Entry]) -> Vec<String> {
        v.iter().map(|e| e.label.clone()).collect()
    }

    #[test]
    fn any_case_finds_every_variant() {
        let d = tree("a");
        for q in ["DEsktop", "desktop", "DESKTOP", "dEsKtOp"] {
            let n = names(&search(&d, q, false, Duration::from_secs(2)));
            for want in ["Desktop/", "desktop/", "DeSKtop/", "DESKTOP-old/", "notes_desktop_backup/"] {
                assert!(n.contains(&want.to_string()), "{q}: missing {want} in {n:?}");
            }
            assert!(!n.iter().any(|x| x.contains(".hidden")), "hidden leaked");
        }
    }

    #[test]
    fn exact_and_prefix_beat_loose_matches() {
        let d = tree("b");
        let n = names(&search(&d, "dsk", false, Duration::from_secs(2)));
        let n = names(&search(&d, "desktop", false, Duration::from_secs(2)));
        let first_loose = n.iter().position(|x| x.starts_with("notes_")).unwrap();
        let last_prefix = n.iter().rposition(|x| x.to_lowercase().starts_with("desktop")).unwrap();
        assert!(last_prefix < first_loose, "{n:?}");
        assert!(rank("desktop", "Desktop/") > rank("desktop", "DESKTOP-old"));
        assert!(rank("desktop", "DESKTOP-old") > rank("desktop", "notes_desktop_backup"));
        assert!(rank("desktop", "notes_desktop_backup") > rank("dsk", "desktop"));
    }

    #[test]
    fn finds_inside_subfolders_but_skips_junk() {
        let d = tree("c");
        let hits = search(&d, "babel", false, Duration::from_secs(2));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].parent, "desktop/scripts");
        assert!(search(&d, "desktop", false, Duration::from_secs(2)).iter().all(|e| !e.path.to_string_lossy().contains("node_modules")));
        assert!(search(&d, ".hidden", false, Duration::from_secs(2)).iter().any(|e| e.label.starts_with(".hidden")));
    }

    #[test]
    fn several_words_and_unicode() {
        let d = tree("d");
        assert_eq!(names(&search(&d, "scripts BABEL", false, Duration::from_secs(2))), ["babel.config.json"]);
        assert!(rank("ДЕСКТОП", "десктоп").is_some_and(|s| s == 1000));
    }
}
