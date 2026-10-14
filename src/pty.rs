//! Runs an editor on a pty we own, so other tui-shim processes can type into it
//! (the overlay uses this to open a file in the editor that is already running).
use anyhow::{bail, Result};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixDatagram;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static WINCH: AtomicBool = AtomicBool::new(false);

extern "C" fn on_winch(_: libc::c_int) {
    WINCH.store(true, Ordering::Relaxed);
}

pub fn socket_path(id: u32) -> PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(dir).join(format!("tui-shim-{id}.sock"))
}

/// Type `bytes` into the editor started by `run` on `sock`.
pub fn inject(sock: &Path, bytes: &[u8]) -> Result<()> {
    UnixDatagram::unbound()?.send_to(bytes, sock)?;
    Ok(())
}

fn copy_size(from: RawFd, to: RawFd) {
    // SAFETY: plain ioctls on fds we own, with a zeroed winsize.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(from, libc::TIOCGWINSZ, &mut ws) == 0 {
            libc::ioctl(to, libc::TIOCSWINSZ, &ws);
        }
    }
}

fn write_all(fd: RawFd, mut buf: &[u8]) {
    while !buf.is_empty() {
        // SAFETY: buf is valid for buf.len() bytes.
        let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
        if n < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        buf = &buf[n as usize..];
    }
}

/// The in-terminal button: a folder badge in the top-right cells (kitty graphics protocol).
/// Clicks on it are taken out of the editor's input and reported on `ctl`.
pub struct Deco {
    pub bg: [u8; 3],
    pub fg: [u8; 3],
    pub ring: [u8; 3],
    pub ctl: PathBuf,
}

/// Where the badge sits, measured in terminal cells. Sizes in pixels are not used: ghostty reports
/// them multiplied by the display scale (on a 1.25 screen the numbers are 1.25 squared too large),
/// while images are drawn in real pixels, so mixing the two put the click zone in the wrong place.
/// A block of whole cells is the same on every display, and the picture is scaled to cover it.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Badge {
    /// the block of cells it covers: columns `col..col+cols`, rows `1..=rows` (1-based)
    pub col: u32,
    pub cols: u32,
    pub rows: u32,
    /// size of the picture to generate: same aspect as the block, so the circle stays round
    pub img_w: u32,
    pub img_h: u32,
    /// transparent pixels above the circle inside the picture
    pub img_top: u32,
}

impl Badge {
    /// inclusive 1-based cell rectangle a click counts on: (col0, col1, row0, row1)
    pub fn hit(&self) -> (u32, u32, u32, u32) {
        (self.col, self.col + self.cols - 1, 1, self.rows)
    }
}

const BADGE_COLS: u32 = 4;
const BADGE_ROWS: u32 = 3; // two rows of circle plus room for the margin above it
/// How far the circle sits below the grid's top edge, in rows. The window's right side carries the padding
/// plus the leftover sliver of the grid (about half a cell on average); this brings the top gap to match.
const TOP_MARGIN_ROWS: f64 = 0.3;

/// A badge in the top-right corner: 4 columns wide, its circle a third of a row below the top.
/// The cell's height:width ratio comes from the reported sizes (the scale cancels out in a ratio);
/// without them cells are assumed twice as tall as wide.
pub fn badge_geometry(cols: u16, rows: u16, xpix: u16, ypix: u16) -> Option<Badge> {
    if (cols as u32) < BADGE_COLS + 2 || (rows as u32) < BADGE_ROWS + 2 {
        return None; // too small a terminal to carry it
    }
    let ratio = if xpix > 0 && ypix > 0 { (ypix as f64 / rows as f64) / (xpix as f64 / cols as f64) } else { 2.0 };
    let ratio = ratio.clamp(1.2, 3.5);
    let img_w = 64.0;
    let row_px = img_w / BADGE_COLS as f64 * ratio; // one row, in image pixels (a column is img_w / 4)
    Some(Badge {
        col: cols as u32 - BADGE_COLS + 1,
        cols: BADGE_COLS,
        rows: BADGE_ROWS,
        img_w: img_w as u32,
        img_h: (row_px * BADGE_ROWS as f64).round() as u32,
        img_top: (row_px * TOP_MARGIN_ROWS).round() as u32,
    })
}

fn winsize(fd: RawFd) -> (u16, u16, u16, u16) {
    // SAFETY: plain ioctl with a zeroed winsize.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) == 0 { (ws.ws_col, ws.ws_row, ws.ws_xpixel, ws.ws_ypixel) } else { (0, 0, 0, 0) }
    }
}

fn current_badge(fd: RawFd) -> Option<Badge> {
    let (c, r, x, y) = winsize(fd);
    badge_geometry(c, r, x, y)
}

const IMG_ID: u32 = 77;
const MARKER_ID: u32 = 78;

fn upload_icon(id: u32, rgba: &[u8], w: u32, h: u32) {
    let b64 = crate::icon::base64(rgba);
    let chunks: Vec<&str> = b64.as_bytes().chunks(4096).map(|c| std::str::from_utf8(c).unwrap()).collect();
    for (i, c) in chunks.iter().enumerate() {
        let more = if i + 1 < chunks.len() { 1 } else { 0 };
        let ctl = if i == 0 { format!("a=t,f=32,s={w},v={h},i={id},q=2,m={more}") } else { format!("m={more}") };
        write_all(1, format!("\x1b_G{ctl};{c}\x1b\\").as_bytes());
    }
}

fn place(id: u32, p: u32, z: u32, b: &Badge) {
    // save cursor, go to the badge's first cell, place (replace) the image over its block of cells, restore
    write_all(1, format!("\x1b7\x1b[1;{}H\x1b_Ga=p,i={id},p={p},c={},r={},C=1,z={z},q=2\x1b\\\x1b8", b.col, b.cols, b.rows).as_bytes());
}

fn draw_icon(b: &Badge) {
    place(IMG_ID, 1, 1000, b);
}

/// Splits mouse reports on the badge from everything else in the terminal's input.
#[derive(Default)]
pub struct InputFilter {
    carry: Vec<u8>,
}

pub struct Filtered {
    pub forward: Vec<u8>,
    /// the badge was clicked with the mouse (the pointer is on it)
    pub clicked: bool,
    /// F9 was pressed
    pub hotkey: bool,
}

impl InputFilter {
    /// `hit` is the badge's cell rectangle (col0, col1, row0, row1); `editor_mouse` says whether the editor asked for mouse input
    /// (otherwise mouse reports are dropped, as the editor never wanted them).
    pub fn feed(&mut self, data: &[u8], hit: (u32, u32, u32, u32), editor_mouse: bool) -> Filtered {
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(data);
        let (mut forward, mut clicked, mut hotkey) = (Vec::with_capacity(buf.len()), false, false);
        let mut i = 0;
        while i < buf.len() {
            // F9 opens the picker too, for editors that do not use the mouse
            if buf[i..].starts_with(b"\x1b[20~") {
                hotkey = true;
                i += 5;
                continue;
            }
            if buf[i] != 0x1b || !buf[i + 1..].starts_with(b"[<") {
                forward.push(buf[i]);
                i += 1;
                continue;
            }
            // ESC [ < Pb ; Px ; Py (M|m)
            let start = i;
            let mut j = i + 3;
            while j < buf.len() && (buf[j].is_ascii_digit() || buf[j] == b';') {
                j += 1;
            }
            if j >= buf.len() {
                self.carry = buf[start..].to_vec(); // report split across reads
                break;
            }
            if buf[j] != b'M' && buf[j] != b'm' {
                forward.push(buf[i]);
                i += 1;
                continue;
            }
            let params: Vec<u32> = std::str::from_utf8(&buf[start + 3..j]).unwrap_or("").split(';').filter_map(|p| p.parse().ok()).collect();
            let on_badge = matches!(params[..], [b, x, y] if b & 0b1100011 == 0 && (hit.0..=hit.1).contains(&x) && (hit.2..=hit.3).contains(&y));
            if on_badge {
                clicked |= buf[j] == b'M';
            } else if editor_mouse {
                forward.extend_from_slice(&buf[start..=j]);
            }
            i = j + 1;
        }
        Filtered { forward, clicked, hotkey }
    }
}

/// Last "set" or "reset" of any of `modes` in `chunk` decides; otherwise the state is unchanged.
fn mode_state(chunk: &[u8], modes: &[&str], current: bool) -> bool {
    let last = |suffix: char| {
        modes.iter().filter_map(|m| {
            let n = format!("\x1b[?{m}{suffix}");
            chunk.windows(n.len()).rposition(|w| w == n.as_bytes())
        }).max()
    };
    match (last('h'), last('l')) {
        (Some(a), Some(b)) => a > b,
        (Some(_), None) => true,
        (None, Some(_)) => false,
        (None, None) => current,
    }
}

/// Does the editor's output switch mouse reporting on (or off)?
fn mouse_state(chunk: &[u8], current: bool) -> bool {
    mode_state(chunk, &["1000", "1002", "1003"], current)
}

/// Does the editor's output contain a kitty-graphics delete command (`ESC _ G ... a=d ... ESC \\`)?
/// Editors with image support (snacks.nvim in LazyVim, for one) clear all images, which also frees
/// the picture we uploaded: placing it afterwards shows nothing until it is sent again.
fn deletes_images(chunk: &[u8]) -> bool {
    let mut from = 0;
    while let Some(i) = chunk[from..].windows(3).position(|w| w == b"\x1b_G") {
        let start = from + i + 3;
        let end = chunk[start..].iter().position(|&b| b == 0x1b).map(|e| start + e).unwrap_or(chunk.len().min(start + 96));
        let keys = &chunk[start..end];
        if keys.split(|&b| b == b',' || b == b';').next().is_some_and(|k| k == b"a=d") || keys.windows(4).any(|w| w == b",a=d" || w == b"a=d,") {
            return true;
        }
        from = start;
    }
    false
}

/// Does the editor's output switch to (or from) the alternate screen? Kitty-protocol terminals
/// keep image storage per screen, so the badge has to be uploaded again after a switch.
fn alt_screen_state(chunk: &[u8], current: bool) -> bool {
    mode_state(chunk, &["1049", "1047", "47"], current)
}

/// Run `argv` on a fresh pty, bridged to our own terminal; returns its exit code.
pub fn run(sock: &Path, argv: &[String], deco: Option<Deco>) -> Result<i32> {
    if argv.is_empty() {
        bail!("nothing to run");
    }
    let (mut m, mut s) = (0, 0);
    // SAFETY: openpty fills the two fds; null optionals are allowed.
    if unsafe { libc::openpty(&mut m, &mut s, std::ptr::null_mut(), std::ptr::null(), std::ptr::null()) } != 0 {
        bail!("openpty failed: {}", std::io::Error::last_os_error());
    }
    // SAFETY: fds just returned by openpty, owned from here on.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s)) };
    copy_size(0, m);

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    // SAFETY: only async-signal-safe calls: new session, slave becomes the controlling tty.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            libc::ioctl(0, libc::TIOCSCTTY, 0);
            // if the wrapper dies (even by SIGKILL) the editor must not live on without a terminal
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGHUP);
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    drop(cmd); // closes our copies of the slave so master sees EOF when the editor exits

    // SAFETY: termios calls on our stdin; original settings are restored before returning.
    let orig = unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        let have = libc::tcgetattr(0, &mut t) == 0;
        if have {
            let mut raw = t;
            libc::cfmakeraw(&mut raw);
            libc::tcsetattr(0, libc::TCSANOW, &raw);
        }
        libc::signal(libc::SIGWINCH, on_winch as *const () as libc::sighandler_t);
        have.then_some(t)
    };

    let _ = std::fs::remove_file(sock);
    let dg = UnixDatagram::bind(sock)?;
    dg.set_nonblocking(true)?;
    let dg_fd = std::os::fd::AsRawFd::as_raw_fd(&dg);
    if let Some(d) = &deco {
        let _ = d; // mouse reporting is left to the editor: forcing it on would take native text selection away
    }
    let ctl = UnixDatagram::unbound()?;
    let (mut filter, mut editor_mouse) = (InputFilter::default(), false);
    let (mut dirty, mut last_out) = (deco.is_some(), Instant::now());
    let (mut alt, mut uploaded): (bool, Option<(bool, u32)>) = (false, None);
    let mut last_winch: Option<Instant> = None;
    let mut dirty_since: Option<Instant> = None;
    let mut last_upload = Instant::now();
    let mut last_refresh = Instant::now();
    let (mut probe_req, mut probe_until, mut marker_uploaded): (bool, Option<Instant>, Option<(bool, u32)>) = (false, None, None);
    let mut buf = [0u8; 8192];
    'outer: loop {
        if WINCH.swap(false, Ordering::Relaxed) {
            copy_size(0, m); // the kernel signals the editor's foreground group itself
            dirty = true;
            last_winch = Some(Instant::now());
            // move the badge in the same instant: the old placement stays on its old cell until
            // it is replaced, which showed it at the previous spot for a moment
            if let (Some(_), Some(b)) = (&deco, current_badge(0)) {
                if uploaded == Some((alt, b.img_h)) {
                    draw_icon(&b);
                }
            }
        }
        let mut fds = [
            libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: m, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: dg_fd, events: libc::POLLIN, revents: 0 },
        ];
        // SAFETY: fds is a valid array of 3 pollfds.
        // wake quickly while the badge is waiting to be redrawn, lazily otherwise
        unsafe { libc::poll(fds.as_mut_ptr(), 3, if dirty { 15 } else { 100 }) };
        if fds[0].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            // SAFETY: buf is valid for its length.
            let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                let _ = child.kill(); // our terminal window went away
                break 'outer;
            }
            if deco.is_some() {
                // fallback when the terminal reports no pixel size: the old two-cell badge in row 1
                // the cells the badge covers; a terminal too small for it has no badge to click
                let hit = current_badge(0).map(|b| b.hit()).unwrap_or((0, 0, 0, 0));
                let f = filter.feed(&buf[..n as usize], hit, editor_mouse);
                write_all(m, &f.forward);
                if f.clicked || f.hotkey {
                    crate::log(&format!("wrapper: badge {} hit={hit:?} badge={:?}", if f.clicked { "click" } else { "F9" }, current_badge(0)));
                    let _ = ctl.send_to(if f.clicked { b"click" } else { b"key" }, &deco.as_ref().unwrap().ctl);
                }
            } else {
                write_all(m, &buf[..n as usize]);
            }
        }
        if fds[2].revents & libc::POLLIN != 0 {
            while let Ok(n) = dg.recv(&mut buf) {
                if &buf[..n] == crate::probe::PROBE {
                    probe_req = true; // not for the editor: flash the marker badge
                } else {
                    write_all(m, &buf[..n]);
                }
            }
        }
        if fds[1].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            // SAFETY: buf is valid for its length.
            let n = unsafe { libc::read(m, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break 'outer; // editor exited
            }
            write_all(1, &buf[..n as usize]);
            editor_mouse = mouse_state(&buf[..n as usize], editor_mouse);
            alt = alt_screen_state(&buf[..n as usize], alt);
            if deletes_images(&buf[..n as usize]) {
                uploaded = None; // our pictures are gone with the rest: send them again at the next draw
                marker_uploaded = None;
                crate::log("the editor deleted all images: re-uploading the badge");
            }
            dirty = deco.is_some();
            last_out = Instant::now();
        }
        // redraw the badge only once the editor has been quiet for a moment, so it never lands
        // in the middle of one of the editor's escape sequences
        // locate request: draw the marker badge (a fill that differs by 3 in blue) over the real one
        if probe_req && last_out.elapsed() >= Duration::from_millis(25) {
            probe_req = false;
            if let (Some(d), Some(b)) = (&deco, current_badge(0)) {
                if marker_uploaded != Some((alt, b.img_h)) {
                    upload_icon(MARKER_ID, &crate::icon::folder_icon(b.img_w as usize, b.img_h as usize, b.img_top as usize, crate::icon::marker_fill(d.bg), d.fg, d.ring), b.img_w, b.img_h);
                    marker_uploaded = Some((alt, b.img_h));
                }
                place(MARKER_ID, 2, 2000, &b);
                let keep = std::env::var("TUI_SHIM_MARKER_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(2500); // invisible, so no cost to keeping it up
                probe_until = Some(Instant::now() + Duration::from_millis(keep));
            }
        }
        if probe_until.is_some_and(|t| Instant::now() >= t) {
            write_all(1, format!("\x1b_Ga=d,d=i,i={MARKER_ID},p=2,q=2\x1b\\").as_bytes());
            probe_until = None;
        }
        // while the window is being resized the editor repaints all the time: redraw in the gaps
        // (a short quiet is enough) so the badge follows the new width instead of waiting for the end
        let resizing = last_winch.is_some_and(|t| t.elapsed() < Duration::from_millis(800));
        let quiet = Duration::from_millis(if resizing { 12 } else { 120 });
        // the picture can vanish without the editor printing anything (the terminal drops it, a
        // workspace switch, ...): while idle, put it back about once a second
        if deco.is_some() && !dirty && last_refresh.elapsed() >= Duration::from_secs(1) {
            dirty = true;
        }
        if dirty && dirty_since.is_none() {
            dirty_since = Some(Instant::now());
        }
        // an editor that repaints all the time never goes quiet: after a while draw between two reads anyway
        let starved = dirty_since.is_some_and(|t| t.elapsed() >= Duration::from_millis(300)) && last_out.elapsed() >= Duration::from_millis(3);
        if dirty && (last_out.elapsed() >= quiet || starved) {
            if let Some(d) = &deco {
                if let Some(b) = current_badge(0) {
                    if uploaded.is_none() {
                        crate::log(&format!("terminal size: cols/rows/xpix/ypix = {:?}, badge = {b:?}", winsize(0)));
                    }
                    // also re-send now and then: if the terminal ever drops the picture for a reason we
                    // cannot see, the badge comes back by itself
                    let stale = last_upload.elapsed() > Duration::from_secs(3);
                    if uploaded != Some((alt, b.img_h)) || stale {
                        last_upload = Instant::now();
                        // image storage is per screen, and the picture's shape follows the cell shape: (re)send
                        upload_icon(IMG_ID, &crate::icon::folder_icon(b.img_w as usize, b.img_h as usize, b.img_top as usize, d.bg, d.fg, d.ring), b.img_w, b.img_h);
                        uploaded = Some((alt, b.img_h));
                    }
                    draw_icon(&b);
                }
            }
            dirty = false;
            dirty_since = None;
            last_refresh = Instant::now();
        }
    }
    if deco.is_some() {
        write_all(1, format!("\x1b_Ga=d,d=I,i={IMG_ID},q=2\x1b\\").as_bytes());
    }
    if let Some(t) = orig {
        // SAFETY: restoring the settings saved above.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) };
    }
    let _ = std::fs::remove_file(sock);
    drop(master);
    Ok(child.wait()?.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// cells a click counts on: columns 79-80, rows 1-2
    const HIT: (u32, u32, u32, u32) = (79, 80, 1, 2);

    #[test]
    fn badge_covers_whole_cells_in_the_top_right_corner() {
        // 80x24 cells of 10x20 px (the pixel numbers only give the cell shape)
        let b = badge_geometry(80, 24, 800, 480).unwrap();
        assert_eq!((b.col, b.cols, b.rows), (77, 4, 3));
        assert_eq!(b.hit(), (77, 80, 1, 3), "the click zone covers the cells the picture covers");
        // the picture has the aspect of the block: 4 cells x 10px by 3 cells x 20px
        assert_eq!((b.img_w, b.img_h), (64, 96));
        assert_eq!(b.img_top, 10, "a third of a row (about 0.3 x 32px) above the circle");
    }

    #[test]
    fn display_scale_does_not_change_the_badge() {
        // the same terminal reported with pixel sizes 1.25^2 too large (what ghostty does at scale 1.25)
        let honest = badge_geometry(135, 48, 1568, 1191).unwrap();
        let inflated = badge_geometry(135, 48, 2444, 1854).unwrap();
        assert_eq!((honest.col, honest.cols, honest.rows), (inflated.col, inflated.cols, inflated.rows));
        assert!((honest.img_h as i32 - inflated.img_h as i32).abs() <= 1, "only the ratio matters");
    }

    #[test]
    fn tiny_terminals_get_no_badge() {
        assert!(badge_geometry(5, 3, 50, 60).is_none());
        // no pixel sizes at all: cells are assumed twice as tall as wide
        assert_eq!(badge_geometry(80, 24, 0, 0).unwrap().img_h, 96);
    }

    #[test]
    fn badge_click_is_taken_and_reported() {
        let mut f = InputFilter::default();
        // left press and release on column 80 of an 80-column terminal, row 1
        let r = f.feed(b"\x1b[<0;80;1M\x1b[<0;80;1m", HIT, true);
        assert!(r.clicked);
        assert!(r.forward.is_empty());
    }

    #[test]
    fn other_mouse_events_pass_through_only_when_editor_wants_them() {
        let mut f = InputFilter::default();
        let r = f.feed(b"\x1b[<0;10;5M", HIT, true);
        assert!(!r.clicked);
        assert_eq!(r.forward, b"\x1b[<0;10;5M");
        let r = f.feed(b"\x1b[<0;10;5M", HIT, false);
        assert!(r.forward.is_empty());
    }

    #[test]
    fn keys_and_other_escapes_are_untouched_and_splits_are_carried() {
        let mut f = InputFilter::default();
        let r = f.feed(b"abc\x1b[A\x1b", HIT, true);
        assert_eq!(r.forward, b"abc\x1b[A\x1b");
        let r = f.feed(b"\x1b[<0;80;", HIT, true);
        assert!(r.forward.is_empty());
        let r = f.feed(b"1M", HIT, true);
        assert!(r.clicked);
    }

    #[test]
    fn f9_opens_the_picker_without_reaching_the_editor() {
        let mut f = InputFilter::default();
        let r = f.feed(b"a\x1b[20~b", HIT, false);
        assert!(r.hotkey && !r.clicked);
        assert_eq!(r.forward, b"ab");
    }

    #[test]
    fn wheel_and_motion_over_the_badge_are_not_clicks() {
        let mut f = InputFilter::default();
        let r = f.feed(b"\x1b[<64;80;1M\x1b[<35;80;1M", HIT, true);
        assert!(!r.clicked);
    }

    #[test]
    fn alt_screen_tracking() {
        assert!(alt_screen_state(b"\x1b[?1049h", false));
        assert!(!alt_screen_state(b"\x1b[?1049h...\x1b[?1049l", false));
        assert!(alt_screen_state(b"plain output", true));
    }

    #[test]
    fn image_delete_commands_are_recognised() {
        assert!(deletes_images(b"text\x1b_Ga=d,d=A\x1b\\more"));
        assert!(deletes_images(b"\x1b_Gq=2,a=d,d=I,i=5\x1b\\"));
        assert!(!deletes_images(b"\x1b_Ga=T,f=32,s=1,v=1;AAAA\x1b\\"), "transmitting is not deleting");
        assert!(!deletes_images(b"\x1b_Ga=q,i=31;AAAA\x1b\\"), "a support query is not deleting");
        assert!(!deletes_images(b"plain output with a=d in it"));
    }

    #[test]
    fn mouse_mode_tracking() {
        assert!(mouse_state(b"x\x1b[?1000h\x1b[?1006h", false));
        assert!(!mouse_state(b"\x1b[?1002l", true));
        assert!(mouse_state(b"nothing", true));
    }
}
