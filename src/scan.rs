use crate::registry::App;
use std::io::Read;
use std::path::{Path, PathBuf};

pub struct Found<'a> {
    pub app: &'a App,
    pub bin: PathBuf,
}

pub fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(bin))
        .find(|p| is_exec(p))
}

fn is_exec(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata().map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

pub fn scan(apps: &[App]) -> Vec<Found<'_>> {
    apps.iter()
        .filter_map(|app| which(&app.name).map(|bin| Found { app, bin }))
        .collect()
}

/// Heuristic: ELF binaries linked against a TUI toolkit that the registry does not know.
pub fn candidates(apps: &[App]) -> Vec<PathBuf> {
    const LIBS: [&[u8]; 4] = [b"libncurses", b"libncursesw", b"libtinfo", b"libnewt"];
    let known: Vec<&str> = apps.iter().map(|a| a.name.as_str()).collect();
    let Some(path) = std::env::var_os("PATH") else { return vec![] };
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for dir in std::env::split_paths(&path) {
        // /bin -> /usr/bin and similar: scan each real directory once
        if !dir.canonicalize().is_ok_and(|d| seen.insert(d)) {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if known.contains(&name.as_str()) || !is_exec(&p) {
                continue;
            }
            // .dynstr (DT_NEEDED names) sits near the start of the file
            let mut buf = Vec::new();
            let Ok(f) = std::fs::File::open(&p) else { continue };
            if f.take(1 << 20).read_to_end(&mut buf).is_err() || !buf.starts_with(b"\x7fELF") {
                continue;
            }
            if LIBS.iter().any(|l| buf.windows(l.len()).any(|w| w == *l)) {
                out.push(p);
            }
        }
    }
    out.sort();
    out.dedup_by_key(|p| p.file_name().map(|n| n.to_owned()));
    out
}
