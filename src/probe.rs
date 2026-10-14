//! Finds the folder badge on screen: the wrapper redraws it for a moment in a marker colour
//! (a fill differing by 3 in blue), a screenshot is taken with `grim` and searched for that colour.
//! This gives the exact position whatever the layout, which niri does not report for tiled windows.
use std::os::unix::net::UnixDatagram;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

/// How long the overlay waits for the whole probe. The default is generous for a normal machine;
/// TUI_SHIM_PROBE_TIMEOUT_MS raises it for slow (software-rendered) test sessions.
pub fn timeout() -> Duration {
    Duration::from_millis(std::env::var("TUI_SHIM_PROBE_TIMEOUT_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(2500))
}

/// Sent to the wrapper's input socket: "draw the marker badge now".
pub const PROBE: &[u8] = b"\0tui-shim-probe\0";

/// Where the badge is, in logical pixels from the top-left of its output.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Found {
    pub right: f64,
    pub top: f64,
}

/// Parse a binary PPM (P6) header; returns (width, height, offset of the pixel data).
fn ppm_header(buf: &[u8]) -> Option<(usize, usize, usize)> {
    let mut pos = 0;
    let mut tok = Vec::new();
    while tok.len() < 4 {
        while pos < buf.len() && buf[pos].is_ascii_whitespace() {
            pos += 1;
        }
        let start = pos;
        while pos < buf.len() && !buf[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if start == pos {
            return None;
        }
        tok.push(std::str::from_utf8(&buf[start..pos]).ok()?.to_string());
    }
    if tok[0] != "P6" {
        return None;
    }
    Some((tok[1].parse().ok()?, tok[2].parse().ok()?, pos + 1))
}

/// Bounding box (left, top, right, bottom) of pixels exactly equal to `fill` in a P6 image.
pub fn find_marker(ppm: &[u8], fill: [u8; 3]) -> Option<(usize, usize, usize, usize)> {
    let (w, h, off) = ppm_header(ppm)?;
    let data = ppm.get(off..off + w * h * 3)?;
    let (mut l, mut t, mut r, mut b) = (usize::MAX, usize::MAX, 0, 0);
    let mut n = 0;
    for (i, px) in data.chunks_exact(3).enumerate() {
        if px == fill {
            let (x, y) = (i % w, i / w);
            l = l.min(x);
            r = r.max(x);
            t = t.min(y);
            b = b.max(y);
            n += 1;
        }
    }
    (n >= 20).then_some((l, t, r, b))
}

fn output_scale(name: &str) -> Option<f64> {
    let out = Command::new("niri").args(["msg", "-j", "outputs"]).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let o = &v[name]["logical"];
    // grim returns the output upright (transform already applied), so the scale is all we need
    o["scale"].as_f64()
}

/// Ask the wrapper on `sock` to flash the marker badge and locate it on `output`.
pub fn locate(sock: &Path, fill: [u8; 3], output: &str) -> Option<Found> {
    let scale = output_scale(output);
    crate::log(&format!("probe: output={output} scale={scale:?} fill={fill:?}"));
    let scale = scale?;
    let sent = UnixDatagram::unbound().ok()?.send_to(PROBE, sock);
    crate::log(&format!("probe: sent to {} -> {sent:?}", sock.display()));
    sent.ok()?;
    // let the terminal draw it (TUI_SHIM_PROBE_DELAY_MS raises this for slow test sessions)
    let delay = std::env::var("TUI_SHIM_PROBE_DELAY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(140);
    std::thread::sleep(Duration::from_millis(delay));
    // the marker may not be on screen yet if the terminal is slow: look twice
    let mut marker = None;
    for attempt in 0..2 {
        let shot = Command::new("grim").args(["-o", output, "-t", "ppm", "-"]).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
        marker = find_marker(&shot.stdout, fill);
        crate::log(&format!("probe: attempt {attempt}: grim exit={:?} bytes={} marker bbox={marker:?}", shot.status.code(), shot.stdout.len()));
        if marker.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    let (l, t, r, _b) = marker?;
    let _ = l;
    // the marker covers the badge's fill; the 2px ring around it is part of the badge
    Some(Found { right: (r as f64 + 3.0) / scale, top: (t as f64 - 2.0) / scale })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ppm(w: usize, h: usize, paint: impl Fn(usize, usize) -> [u8; 3]) -> Vec<u8> {
        let mut v = format!("P6\n{w} {h}\n255\n").into_bytes();
        for y in 0..h {
            for x in 0..w {
                v.extend_from_slice(&paint(x, y));
            }
        }
        v
    }

    #[test]
    fn finds_the_marker_block() {
        let img = ppm(100, 60, |x, y| if (70..90).contains(&x) && (10..30).contains(&y) { [1, 2, 3] } else { [9, 9, 9] });
        assert_eq!(find_marker(&img, [1, 2, 3]), Some((70, 10, 89, 29)));
    }

    #[test]
    fn ignores_stray_pixels_and_missing_marker() {
        let img = ppm(50, 50, |x, y| if x == 3 && y == 4 { [1, 2, 3] } else { [0, 0, 0] });
        assert_eq!(find_marker(&img, [1, 2, 3]), None, "a couple of pixels is not a badge");
        assert_eq!(find_marker(b"not an image", [1, 2, 3]), None);
    }

    #[test]
    fn marker_differs_from_the_fill_only_slightly() {
        let m = crate::icon::marker_fill([218, 212, 193]);
        assert_eq!(m, [218, 212, 196]);
        assert_eq!(crate::icon::marker_fill([0, 0, 255]), [0, 0, 252]);
    }
}
