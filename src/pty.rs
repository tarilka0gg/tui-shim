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

/// Where the badge sits, in the terminal's cells and pixels.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Badge {
    /// image edge in pixels
    pub n: u32,
    /// 1-based cell holding the image's top-left corner, and the pixel offset inside that cell
    pub col: u32,
    pub row: u32,
    pub xo: u32,
    pub yo: u32,
    /// inclusive 1-based cell rectangle a click counts on
    pub hit: (u32, u32, u32, u32),
}

/// Badge about 1.6 cells tall, half a cell (about 10px) away from the top and right edges of the
/// terminal grid. None when the terminal does not report pixel sizes.
pub fn badge_geometry(cols: u16, rows: u16, xpix: u16, ypix: u16) -> Option<Badge> {
    if cols == 0 || rows == 0 || xpix == 0 || ypix == 0 {
        return None;
    }
    let (cw, ch) = (xpix as f64 / cols as f64, ypix as f64 / rows as f64);
    let n = (ch * 1.6).round();
    let gap = (ch * 0.5).round();
    let (x, y) = (xpix as f64 - gap - n, gap);
    let (col0, row0) = ((x / cw).floor(), (y / ch).floor());
    let (xo, yo) = ((x - col0 * cw).round().min(cw.floor() - 1.0).max(0.0), (y - row0 * ch).round().min(ch.floor() - 1.0).max(0.0));
    let c1 = ((x + n - 1.0) / cw).floor() + 1.0;
    let r1 = ((y + n - 1.0) / ch).floor() + 1.0;
    Some(Badge { n: n as u32, col: col0 as u32 + 1, row: row0 as u32 + 1, xo: xo as u32, yo: yo as u32, hit: (col0 as u32 + 1, c1 as u32, row0 as u32 + 1, r1 as u32) })
}

fn winsize(fd: RawFd) -> (u16, u16, u16, u16) {
    // SAFETY: plain ioctl with a zeroed winsize.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) == 0 { (ws.ws_col, ws.ws_row, ws.ws_xpixel, ws.ws_ypixel) } else { (0, 0, 0, 0) }
    }
}

fn cols_of(fd: RawFd) -> u16 {
    winsize(fd).0
}

fn current_badge(fd: RawFd) -> Option<Badge> {
    let (c, r, x, y) = winsize(fd);
    badge_geometry(c, r, x, y)
}

const IMG_ID: u32 = 77;
const MARKER_ID: u32 = 78;

fn upload_icon(id: u32, rgba: &[u8], px: u32) {
    let b64 = crate::icon::base64(rgba);
    let chunks: Vec<&str> = b64.as_bytes().chunks(4096).map(|c| std::str::from_utf8(c).unwrap()).collect();
    for (i, c) in chunks.iter().enumerate() {
        let more = if i + 1 < chunks.len() { 1 } else { 0 };
        let ctl = if i == 0 { format!("a=t,f=32,s={px},v={px},i={id},q=2,m={more}") } else { format!("m={more}") };
        write_all(1, format!("\x1b_G{ctl};{c}\x1b\\").as_bytes());
    }
}

fn draw_icon(b: &Badge) {
    // save cursor, go to the badge's cell, place (replace) the image at its pixel offset, restore
    write_all(1, format!("\x1b7\x1b[{};{}H\x1b_Ga=p,i={IMG_ID},p=1,X={},Y={},C=1,z=1000,q=2\x1b\\\x1b8", b.row, b.col, b.xo, b.yo).as_bytes());
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
    let (mut probe_req, mut probe_until, mut marker_uploaded): (bool, Option<Instant>, Option<(bool, u32)>) = (false, None, None);
    let mut buf = [0u8; 8192];
    'outer: loop {
        if WINCH.swap(false, Ordering::Relaxed) {
            copy_size(0, m); // the kernel signals the editor's foreground group itself
            dirty = true;
            last_winch = Some(Instant::now());
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
                let hit = current_badge(0).map(|b| b.hit).unwrap_or_else(|| {
                    let c = cols_of(0) as u32;
                    (c.saturating_sub(1), c, 1, 1)
                });
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
            dirty = deco.is_some();
            last_out = Instant::now();
        }
        // redraw the badge only once the editor has been quiet for a moment, so it never lands
        // in the middle of one of the editor's escape sequences
        // locate request: draw the marker badge (a fill that differs by 3 in blue) over the real one
        if probe_req && last_out.elapsed() >= Duration::from_millis(25) {
            probe_req = false;
            if let (Some(d), Some(b)) = (&deco, current_badge(0)) {
                if marker_uploaded != Some((alt, b.n)) {
                    upload_icon(MARKER_ID, &crate::icon::folder_icon(b.n as usize, crate::icon::marker_fill(d.bg), d.fg, d.ring), b.n);
                    marker_uploaded = Some((alt, b.n));
                }
                write_all(1, format!("\x1b7\x1b[{};{}H\x1b_Ga=p,i={MARKER_ID},p=2,X={},Y={},C=1,z=2000,q=2\x1b\\\x1b8", b.row, b.col, b.xo, b.yo).as_bytes());
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
        if dirty && dirty_since.is_none() {
            dirty_since = Some(Instant::now());
        }
        // an editor that repaints all the time never goes quiet: after a while draw between two reads anyway
        let starved = dirty_since.is_some_and(|t| t.elapsed() >= Duration::from_millis(300)) && last_out.elapsed() >= Duration::from_millis(3);
        if dirty && (last_out.elapsed() >= quiet || starved) {
            if let Some(d) = &deco {
                match current_badge(0) {
                    Some(b) => {
                        if uploaded != Some((alt, b.n)) {
                            // image storage is per screen, and the size follows the font: (re)send
                            upload_icon(IMG_ID, &crate::icon::folder_icon(b.n as usize, d.bg, d.fg, d.ring), b.n);
                            uploaded = Some((alt, b.n));
                        }
                        draw_icon(&b);
                    }
                    None => {
                        // no pixel size known: two cells wide, one tall, in the corner
                        if uploaded != Some((alt, 0)) {
                            upload_icon(IMG_ID, &crate::icon::folder_icon(64, d.bg, d.fg, d.ring), 64);
                            uploaded = Some((alt, 0));
                        }
                        let col = cols_of(0).saturating_sub(1).max(1);
                        write_all(1, format!("\x1b7\x1b[1;{col}H\x1b_Ga=p,i={IMG_ID},p=1,c=2,r=1,C=1,z=1000,q=2\x1b\\\x1b8").as_bytes());
                    }
                }
            }
            dirty = false;
            dirty_since = None;
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
    fn badge_geometry_keeps_a_half_cell_gap_to_the_top_and_right() {
        // 80x24 cells of 10x20 px
        let b = badge_geometry(80, 24, 800, 480).unwrap();
        assert_eq!(b.n, 32); // 1.6 cells tall
        let (x, y) = ((b.col - 1) * 10 + b.xo, (b.row - 1) * 20 + b.yo);
        assert_eq!(800 - (x + b.n), 10, "10px from the right edge");
        assert_eq!(y, 10, "10px from the top edge");
        assert!(b.hit.0 <= b.col && b.hit.1 >= 80 - 2 && b.hit.2 == 1);
        assert!(badge_geometry(80, 24, 0, 0).is_none());
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
    fn mouse_mode_tracking() {
        assert!(mouse_state(b"x\x1b[?1000h\x1b[?1006h", false));
        assert!(!mouse_state(b"\x1b[?1002l", true));
        assert!(mouse_state(b"nothing", true));
    }
}
