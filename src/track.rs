//! Follows one editor window through niri IPC so the overlay can pin itself to it.
use serde_json::Value;
use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    /// margins are from the monitor's top-right corner, in logical pixels
    Show { right: i32, top: i32, /** x of the window's right edge; 0 when unknown */ edge_x: i32, /** position worked out, not a corner fallback */ exact: bool, monitor: Option<String> },
    /// the in-terminal badge was clicked (the pointer is on it); when the click arrived
    Click(Instant),
    /// F9 was pressed in the editor's terminal; when it arrived
    Hotkey(Instant),
    Hide,
    Quit,
}

fn niri_json(args: &[&str]) -> Option<Value> {
    let out = Command::new("niri").arg("msg").arg("-j").args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    serde_json::from_slice(&out.stdout).ok()
}

pub fn windows() -> Vec<Value> {
    niri_json(&["windows"]).and_then(|v| v.as_array().cloned()).unwrap_or_default()
}

pub fn window_ids(class: &str) -> HashSet<u64> {
    windows().iter().filter(|w| w["app_id"] == class).filter_map(|w| w["id"].as_u64()).collect()
}

/// Distance of the button from the window's top-right corner.
const INSET: i64 = 10;
/// niri's default gap, used when it cannot be inferred from the layout.
const DEFAULT_GAP: i64 = 16;

fn gap_file() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config/tui-shim/gap"))
}

/// The gap size last inferred from a layout that filled the screen (niri's default if never seen).
fn saved_gap() -> i64 {
    gap_file().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| t.trim().parse().ok()).unwrap_or(DEFAULT_GAP)
}

fn save_gap(g: i64) {
    if g == saved_gap() {
        return;
    }
    if let Some(p) = gap_file() {
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        let _ = std::fs::write(p, g.to_string());
    }
}

/// niri reports no position for tiled windows, but it does report each window's column,
/// row and size. If the whole strip of columns fits on the monitor it is not scrolled, so the
/// position follows from the widths and the gap (inferred when the columns fill the screen,
/// else the last inferred value). If the strip overflows it is scrolled by an amount niri does
/// not report (a column can end up anywhere, even with empty space beside it): return None and
/// let the caller find the window another way.
fn tiled_pos(w: &Value, all: &[Value], out_w: i64) -> Option<(i64, i64)> {
    use std::collections::BTreeMap;
    let pos = &w["layout"]["pos_in_scrolling_layout"];
    let (col, row) = (pos[0].as_i64()?, pos[1].as_i64()?);
    let mut cols: BTreeMap<i64, (i64, Vec<(i64, i64)>)> = BTreeMap::new();
    for v in all.iter().filter(|v| v["workspace_id"] == w["workspace_id"] && v["is_floating"] == false) {
        let p = &v["layout"]["pos_in_scrolling_layout"];
        let (Some(c), Some(r), Some(tw), Some(th)) = (p[0].as_i64(), p[1].as_i64(), v["layout"]["tile_size"][0].as_f64(), v["layout"]["tile_size"][1].as_f64()) else { continue };
        let e = cols.entry(c).or_insert((0, vec![]));
        e.0 = e.0.max(tw.round() as i64);
        e.1.push((r, th.round() as i64));
    }
    let n = cols.len() as i64;
    let sum: i64 = cols.values().map(|c| c.0).sum();
    let my_w = cols.get(&col)?.0;
    let inferred = if n > 1 {
        (0..=40).find(|g| (sum + (n + 1) * g - out_w).abs() <= 2)
    } else if (0..=80).contains(&(out_w - my_w)) && (out_w - my_w) % 2 == 0 {
        Some((out_w - my_w) / 2) // one column filling the screen
    } else {
        None
    };
    if let Some(g) = inferred {
        save_gap(g);
    }
    let gap = inferred.unwrap_or_else(saved_gap);
    let overflow = sum + (n + 1) * gap > out_w + 2;
    if overflow {
        return None;
    }
    let x = gap + cols.range(..col).map(|(_, c)| c.0 + gap).sum::<i64>();
    let mut rows = cols.get(&col)?.1.clone();
    rows.sort();
    let y = gap + rows.iter().filter(|(r, _)| *r < row).map(|(_, h)| h + gap).sum::<i64>();
    Some((x, y))
}

fn state_for(id: u64) -> Msg {
    let all = windows();
    let Some(w) = all.iter().find(|w| w["id"].as_u64() == Some(id)) else { return Msg::Quit };
    if !w["is_focused"].as_bool().unwrap_or(false) {
        return Msg::Hide;
    }
    let ws = niri_json(&["workspaces"]).and_then(|v| v.as_array().cloned()).unwrap_or_default();
    let monitor = ws.iter().find(|x| x["id"] == w["workspace_id"]).and_then(|x| x["output"].as_str().map(String::from));
    let out_w = monitor.as_deref().and_then(|m| niri_json(&["outputs"]).map(|o| o[m]["logical"]["width"].as_i64())).flatten();
    // default: monitor's top-right corner (used when the position cannot be worked out)
    let (mut right, mut top, mut edge_x, mut exact) = (INSET, INSET, 0, false);
    if let Some(ow) = out_w {
        let size = &w["layout"]["tile_size"];
        let tile_w = size[0].as_f64().unwrap_or(0.0).round() as i64;
        let spot = if w["is_floating"].as_bool() == Some(true) {
            let p = &w["layout"]["tile_pos_in_workspace_view"];
            p[0].as_f64().zip(p[1].as_f64()).map(|(x, y)| (x.round() as i64, y.round() as i64))
        } else {
            tiled_pos(w, &all, ow)
        };
        if let (Some((x, y)), true) = (spot, tile_w > 0) {
            right = (ow - (x + tile_w)).max(0) + INSET;
            top = y + INSET;
            edge_x = x + tile_w;
            exact = true;
        }
    }
    Msg::Show { right: right as i32, top: top as i32, edge_x: edge_x as i32, exact, monitor }
}

/// Pid of `pid`'s parent, from /proc.
fn ppid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // the command name (field 2) may contain spaces; fields after the last ')' are fixed
    stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()
}

/// The niri window that hosts this process's terminal: a window whose pid is one of our
/// ancestors (the terminal emulator), preferring the focused one (we were just typed in it).
pub fn own_window_id() -> Option<u64> {
    // the shell starts a moment before niri lists the new window, so retry briefly
    for _ in 0..40 {
        if let Some(id) = own_window_once() {
            return Some(id);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

fn own_window_once() -> Option<u64> {
    let mut anc = HashSet::new();
    let mut p = std::process::id();
    while p > 1 && anc.insert(p) {
        match ppid(p) {
            Some(parent) => p = parent,
            None => break,
        }
    }
    let mine: Vec<Value> = windows().into_iter().filter(|w| w["pid"].as_u64().is_some_and(|x| anc.contains(&(x as u32)))).collect();
    mine.iter().find(|w| w["is_focused"] == true).or(mine.first()).and_then(|w| w["id"].as_u64())
}

/// Waits for the editor's window to appear (a new window of `class`), then follows it.
pub fn spawn(class: String, before: HashSet<u64>, tx: Sender<Msg>) {
    std::thread::spawn(move || {
        let t = Instant::now();
        let id = loop {
            if let Some(id) = windows().iter().filter(|w| w["app_id"] == class).filter_map(|w| w["id"].as_u64()).find(|i| !before.contains(i)) {
                break id;
            }
            if t.elapsed() > Duration::from_secs(20) {
                let _ = tx.send(Msg::Quit);
                return;
            }
            std::thread::sleep(Duration::from_millis(150));
        };
        follow(id, tx);
    });
}

/// Follow a window that already exists (an editor started inside the current terminal).
pub fn follow_window(id: u64, tx: Sender<Msg>) {
    std::thread::spawn(move || follow(id, tx));
}

/// Streams Msg for window `id` until it closes.
fn follow(id: u64, tx: Sender<Msg>) {
    let (ev_tx, ev_rx): (Sender<()>, Receiver<()>) = mpsc::channel();
    let child = Command::new("niri").args(["msg", "-j", "event-stream"]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(_) => {
            let _ = tx.send(Msg::Quit);
            return;
        }
    };
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if (line.contains("Window") || line.contains("Workspace")) && ev_tx.send(()).is_err() {
                break;
            }
        }
    });
    let mut last: Option<Msg> = None;
    loop {
        let msg = state_for(id);
        if std::env::var_os("TUI_SHIM_DEBUG").is_some() {
            let w = windows().into_iter().find(|w| w["id"].as_u64() == Some(id));
            eprintln!("track: {msg:?} layout={:?}", w.map(|w| w["layout"].clone()));
        }
        if last.as_ref() != Some(&msg) {
            if tx.send(msg.clone()).is_err() || msg == Msg::Quit {
                break;
            }
            last = Some(msg);
        }
        // wait for the next event; the timeout is a safety net, not the main path
        match ev_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(()) => {
                std::thread::sleep(Duration::from_millis(40));
                while ev_rx.try_recv().is_ok() {}
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = tx.send(Msg::Quit);
                break;
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}
