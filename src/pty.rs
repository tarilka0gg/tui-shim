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
    pub rgba: Vec<u8>,
    pub px: usize,
    pub ctl: PathBuf,
}

const IMG_ID: u32 = 77;

fn upload_icon(d: &Deco) {
    let b64 = crate::icon::base64(&d.rgba);
    let chunks: Vec<&str> = b64.as_bytes().chunks(4096).map(|c| std::str::from_utf8(c).unwrap()).collect();
    for (i, c) in chunks.iter().enumerate() {
        let more = if i + 1 < chunks.len() { 1 } else { 0 };
        let ctl = if i == 0 { format!("a=t,f=32,s={px},v={px},i={IMG_ID},q=2,m={more}", px = d.px) } else { format!("m={more}") };
        write_all(1, format!("\x1b_G{ctl};{c}\x1b\\").as_bytes());
    }
}

fn draw_icon(cols: u16) {
    // save cursor, go to the top-right two cells, place (replace) the image, restore the cursor
    let col = cols.saturating_sub(1).max(1);
    write_all(1, format!("\x1b7\x1b[1;{col}H\x1b_Ga=p,i={IMG_ID},p=1,c=2,r=1,C=1,z=1000,q=2\x1b\\\x1b8").as_bytes());
}

fn cols_of(fd: RawFd) -> u16 {
    // SAFETY: plain ioctl with a zeroed winsize.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) == 0 { ws.ws_col } else { 0 }
    }
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
    /// `cols` is the terminal width; `editor_mouse` says whether the editor asked for mouse input
    /// (otherwise mouse reports are dropped, as the editor never wanted them).
    pub fn feed(&mut self, data: &[u8], cols: u16, editor_mouse: bool) -> Filtered {
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
            let on_badge = matches!(params[..], [b, x, 1] if b & 0b1100011 == 0 && x + 1 >= cols as u32 && x <= cols as u32);
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
    let (mut alt, mut uploaded_on_alt): (bool, Option<bool>) = (false, None);
    let mut buf = [0u8; 8192];
    'outer: loop {
        if WINCH.swap(false, Ordering::Relaxed) {
            copy_size(0, m); // the kernel signals the editor's foreground group itself
            dirty = true;
        }
        let mut fds = [
            libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: m, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: dg_fd, events: libc::POLLIN, revents: 0 },
        ];
        // SAFETY: fds is a valid array of 3 pollfds.
        unsafe { libc::poll(fds.as_mut_ptr(), 3, 100) };
        if fds[0].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            // SAFETY: buf is valid for its length.
            let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                let _ = child.kill(); // our terminal window went away
                break 'outer;
            }
            if deco.is_some() {
                let f = filter.feed(&buf[..n as usize], cols_of(0), editor_mouse);
                write_all(m, &f.forward);
                if f.clicked || f.hotkey {
                    crate::log(&format!("wrapper: badge {} (cols={})", if f.clicked { "click" } else { "F9" }, cols_of(0)));
                    let _ = ctl.send_to(if f.clicked { b"click" } else { b"key" }, &deco.as_ref().unwrap().ctl);
                }
            } else {
                write_all(m, &buf[..n as usize]);
            }
        }
        if fds[2].revents & libc::POLLIN != 0 {
            while let Ok(n) = dg.recv(&mut buf) {
                write_all(m, &buf[..n]);
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
        if dirty && last_out.elapsed() >= Duration::from_millis(120) {
            if let Some(d) = &deco {
                if uploaded_on_alt != Some(alt) {
                    upload_icon(d); // image storage is per screen: (re)send after a screen switch
                    uploaded_on_alt = Some(alt);
                }
            }
            draw_icon(cols_of(0));
            dirty = false;
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

    #[test]
    fn badge_click_is_taken_and_reported() {
        let mut f = InputFilter::default();
        // left press and release on column 80 of an 80-column terminal, row 1
        let r = f.feed(b"\x1b[<0;80;1M\x1b[<0;80;1m", 80, true);
        assert!(r.clicked);
        assert!(r.forward.is_empty());
    }

    #[test]
    fn other_mouse_events_pass_through_only_when_editor_wants_them() {
        let mut f = InputFilter::default();
        let r = f.feed(b"\x1b[<0;10;5M", 80, true);
        assert!(!r.clicked);
        assert_eq!(r.forward, b"\x1b[<0;10;5M");
        let r = f.feed(b"\x1b[<0;10;5M", 80, false);
        assert!(r.forward.is_empty());
    }

    #[test]
    fn keys_and_other_escapes_are_untouched_and_splits_are_carried() {
        let mut f = InputFilter::default();
        let r = f.feed(b"abc\x1b[A\x1b", 80, true);
        assert_eq!(r.forward, b"abc\x1b[A\x1b");
        let r = f.feed(b"\x1b[<0;80;", 80, true);
        assert!(r.forward.is_empty());
        let r = f.feed(b"1M", 80, true);
        assert!(r.clicked);
    }

    #[test]
    fn f9_opens_the_picker_without_reaching_the_editor() {
        let mut f = InputFilter::default();
        let r = f.feed(b"a\x1b[20~b", 80, false);
        assert!(r.hotkey && !r.clicked);
        assert_eq!(r.forward, b"ab");
    }

    #[test]
    fn wheel_and_motion_over_the_badge_are_not_clicks() {
        let mut f = InputFilter::default();
        let r = f.feed(b"\x1b[<64;80;1M\x1b[<35;80;1M", 80, true);
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
