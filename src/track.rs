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
    Show { right: i32, top: i32, monitor: Option<String> },
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

const GAP: i64 = 8;

fn state_for(id: u64) -> Msg {
    let Some(w) = windows().into_iter().find(|w| w["id"].as_u64() == Some(id)) else { return Msg::Quit };
    if !w["is_focused"].as_bool().unwrap_or(false) {
        return Msg::Hide;
    }
    let ws = niri_json(&["workspaces"]).and_then(|v| v.as_array().cloned()).unwrap_or_default();
    let monitor = ws.iter().find(|x| x["id"] == w["workspace_id"]).and_then(|x| x["output"].as_str().map(String::from));
    let out_w = monitor
        .as_deref()
        .and_then(|m| niri_json(&["outputs"]).map(|o| o[m]["logical"]["width"].as_i64()))
        .flatten();
    let (mut right, mut top) = (GAP, GAP);
    // niri only reports a position for floating windows; tiled ones pin to the monitor corner
    if w["is_floating"].as_bool() == Some(true) {
        let (pos, size) = (&w["layout"]["tile_pos_in_workspace_view"], &w["layout"]["window_size"]);
        if let (Some(x), Some(y), Some(ww), Some(ow)) = (pos[0].as_f64(), pos[1].as_f64(), size[0].as_i64(), out_w) {
            right = (ow - (x as i64 + ww)).max(0) + GAP;
            top = y as i64 + GAP;
        }
    }
    Msg::Show { right: right as i32, top: top as i32, monitor }
}

/// Waits for the editor's window to appear (a new window of `class`), then streams Msg
/// until it closes. Sends Quit when the window is gone or never showed up.
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
                if line.contains("Window") || line.contains("Workspace") {
                    if ev_tx.send(()).is_err() {
                        break;
                    }
                }
            }
        });
        let mut last: Option<Msg> = None;
        loop {
            let msg = state_for(id);
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
    });
}
