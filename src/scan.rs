use crate::registry::App;
use std::io::Read;
use std::path::{Path, PathBuf};

pub struct Found<'a> {
    pub app: &'a App,
    pub bin: PathBuf,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Origin {
    System,
    User,
}

impl std::fmt::Display for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self { Origin::System => "system", Origin::User => "user" })
    }
}

#[derive(Clone, Debug)]
pub struct Variant {
    pub path: PathBuf,
    pub origin: Origin,
}

/// Every distinct executable called `name`, in lookup order. $PATH plus the usual
/// user-install dirs, deduplicated across merged-usr symlinks (/bin -> /usr/bin).
pub fn variants(name: &str, home: &Path) -> Vec<Variant> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    dirs.extend([home.join(".local/bin"), home.join(".cargo/bin"), PathBuf::from("/usr/local/bin")]);
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for d in dirs {
        let Ok(real) = d.canonicalize() else { continue };
        let p = real.join(name);
        if is_exec(&p) && seen.insert(p.clone()) {
            let origin = if p.starts_with(home) { Origin::User } else { Origin::System };
            out.push(Variant { path: p, origin });
        }
    }
    out
}

/// First line of `bin <arg>`, killed after 2s. "?" when unavailable.
pub fn version(bin: &Path, arg: &str) -> String {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    let mut cmd = Command::new(bin);
    cmd.arg(arg).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    // New session: no controlling tty. Some TUIs (yazi) probe /dev/tty even for --version,
    // which would print escape codes into our picker and swallow its keystrokes.
    // SAFETY: setsid is async-signal-safe and the closure touches no other state.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let Ok(mut c) = cmd.spawn() else {
        return "?".into();
    };
    let t = Instant::now();
    loop {
        match c.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if t.elapsed() < Duration::from_secs(2) => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = c.kill();
                let _ = c.wait();
                break;
            }
        }
    }
    let mut s = String::new();
    if let Some(o) = c.stdout.take() {
        let _ = o.take(4096).read_to_string(&mut s);
    }
    s.lines().find(|l| !l.trim().is_empty()).map(|l| l.trim().chars().take(70).collect()).unwrap_or_else(|| "?".into())
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

/// Installed registry apps. A saved choice wins when that binary still exists,
/// otherwise the first copy in lookup order.
pub fn scan<'a>(apps: &'a [App], home: &Path) -> Vec<Found<'a>> {
    let choices = crate::choices::Choices::load(home);
    apps.iter()
        .filter_map(|app| {
            let vs = variants(&app.name, home);
            let chosen = choices
                .bin
                .get(&app.name)
                .and_then(|c| vs.iter().find(|v| v.path.to_str() == Some(c.as_str())));
            chosen.or(vs.first()).map(|v| Found { app, bin: v.path.clone() })
        })
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
