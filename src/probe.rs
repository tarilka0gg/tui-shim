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

/// Sent to the wrapper's input socket: "draw the marker badge now". Anything after the prefix is the
/// path of a socket to answer on once the marker has been drawn.
pub const PROBE: &[u8] = b"\0tui-shim-probe\0";

/// Where the badge is, in logical pixels from the top-left of its output.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Found {
    pub right: f64,
    pub top: f64,
    /// diameter of the badge circle
    pub size: f64,
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

/// Bounding box (left, top, right, bottom) of the badge in a P6 image: the connected blob of
/// pixels exactly equal to `fill` that looks like it. The colour alone is not enough (a wallpaper
/// can contain the same colour anywhere), so candidates are grouped into connected shapes and
/// judged on size, squareness and fill: a disc with a folder cut out of it, not a solid area.
pub fn find_marker(ppm: &[u8], fill: [u8; 3]) -> Option<(usize, usize, usize, usize)> {
    let (w, h, off) = ppm_header(ppm)?;
    let data = ppm.get(off..off + w * h * 3)?;
    let is_match = |i: usize| data[i * 3..i * 3 + 3] == fill;
    let mut seen = vec![false; w * h];
    let mut best: Option<(usize, (usize, usize, usize, usize))> = None; // (pixel count, bbox)
    let mut stack: Vec<usize> = Vec::new();
    for start in 0..w * h {
        if seen[start] || !is_match(start) {
            continue;
        }
        // flood fill one blob
        let (mut l, mut t, mut r, mut b, mut n) = (usize::MAX, usize::MAX, 0, 0, 0);
        seen[start] = true;
        stack.push(start);
        while let Some(i) = stack.pop() {
            let (x, y) = (i % w, i / w);
            l = l.min(x);
            r = r.max(x);
            t = t.min(y);
            b = b.max(y);
            n += 1;
            let mut visit = |j: usize| {
                if !seen[j] && is_match(j) {
                    seen[j] = true;
                    stack.push(j);
                }
            };
            if x > 0 {
                visit(i - 1);
            }
            if x + 1 < w {
                visit(i + 1);
            }
            if y > 0 {
                visit(i - w);
            }
            if y + 1 < h {
                visit(i + w);
            }
        }
        let (bw, bh) = (r - l + 1, b - t + 1);
        let squareish = bw * 10 >= bh * 8 && bh * 10 >= bw * 8; // within 20%
        let fill_ratio = n as f64 / (bw * bh) as f64; // a disc is ~0.78, with the folder cut out less
        if (14..=240).contains(&bw) && squareish && (0.3..=0.9).contains(&fill_ratio) && best.is_none_or(|(m, _)| n > m) {
            best = Some((n, (l, t, r, b)));
        }
    }
    best.map(|(_, bbox)| bbox)
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
    // the wrapper answers on this socket as soon as the marker is drawn, so we need not guess how
    // long a busy editor takes to settle
    let reply = std::env::temp_dir().join(format!("tui-shim-probe-{}-{}.reply", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0)));
    let reply_dir = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from).map(|d| d.join(reply.file_name().unwrap())).unwrap_or(reply);
    let _ = std::fs::remove_file(&reply_dir);
    let listener = UnixDatagram::bind(&reply_dir).ok();
    if let Some(l) = &listener {
        let _ = l.set_read_timeout(Some(Duration::from_millis(900)));
    }
    let mut request = PROBE.to_vec();
    request.extend_from_slice(reply_dir.as_os_str().as_encoded_bytes());
    let sent = UnixDatagram::unbound().ok()?.send_to(&request, sock);
    crate::log(&format!("probe: sent to {} -> {sent:?}", sock.display()));
    sent.ok()?;
    let acked = listener.as_ref().is_some_and(|l| l.recv(&mut [0u8; 8]).is_ok());
    let _ = std::fs::remove_file(&reply_dir);
    // a moment for the terminal to put the marker on screen after the wrapper wrote it
    // (TUI_SHIM_PROBE_DELAY_MS raises this for slow, software-rendered test sessions)
    let delay = std::env::var("TUI_SHIM_PROBE_DELAY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(if acked { 45 } else { 200 });
    crate::log(&format!("probe: wrapper answered={acked}, waiting {delay}ms for the frame"));
    std::thread::sleep(Duration::from_millis(delay));
    // look a few times, asking again each time; the caller gives up after its own overall time limit
    let mut marker = None;
    for attempt in 0..4 {
        if attempt > 0 {
            let _ = UnixDatagram::unbound().ok()?.send_to(PROBE, sock); // no reply wanted this time
        }
        let shot = Command::new("grim").args(["-o", output, "-t", "ppm", "-"]).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
        marker = find_marker(&shot.stdout, fill);
        crate::log(&format!("probe: attempt {attempt}: grim exit={:?} bytes={} marker bbox={marker:?}", shot.status.code(), shot.stdout.len()));
        if marker.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    let (l, t, r, _b) = marker?;
    // the marker covers the badge's fill; the 2px ring around it is part of the badge
    Some(Found { right: (r as f64 + 3.0) / scale, top: (t as f64 - 2.0) / scale, size: ((r - l) as f64 + 5.0) / scale })
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

    /// a round badge (disc with a hole in the middle) at (cx, cy), radius r
    fn disc(x: usize, y: usize, cx: usize, cy: usize, r: usize) -> bool {
        let (dx, dy) = (x as i64 - cx as i64, y as i64 - cy as i64);
        let d2 = (dx * dx + dy * dy) as usize;
        d2 <= r * r && d2 >= (r / 3) * (r / 3) // the folder glyph is cut out of the middle
    }

    #[test]
    fn finds_the_badge_blob() {
        let img = ppm(200, 120, |x, y| if disc(x, y, 150, 30, 20) { [1, 2, 3] } else { [9, 9, 9] });
        assert_eq!(find_marker(&img, [1, 2, 3]), Some((130, 10, 170, 50)));
    }

    #[test]
    fn a_wallpaper_with_the_same_colour_does_not_fool_it() {
        // the colour also fills a large solid area and scattered specks; the badge is the small round blob
        let img = ppm(300, 200, |x, y| {
            let solid = (10..120).contains(&x) && (100..190).contains(&y); // big flat area of the same colour
            let specks = (x * 7 + y * 13) % 41 == 0; // noise
            if disc(x, y, 250, 40, 22) || solid || specks { [1, 2, 3] } else { [9, 9, 9] }
        });
        assert_eq!(find_marker(&img, [1, 2, 3]), Some((228, 18, 272, 62)));
    }

    #[test]
    fn nothing_badge_like_means_no_answer() {
        let img = ppm(80, 80, |x, y| if x == 3 && y == 4 { [1, 2, 3] } else { [0, 0, 0] });
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
