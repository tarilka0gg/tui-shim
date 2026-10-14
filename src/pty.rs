//! Runs an editor on a pty we own, so other tui-shim processes can type into it
//! (the overlay uses this to open a file in the editor that is already running).
use anyhow::{bail, Result};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixDatagram;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

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

/// Run `argv` on a fresh pty, bridged to our own terminal; returns its exit code.
pub fn run(sock: &Path, argv: &[String]) -> Result<i32> {
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
    let mut buf = [0u8; 8192];
    'outer: loop {
        if WINCH.swap(false, Ordering::Relaxed) {
            copy_size(0, m); // the kernel signals the editor's foreground group itself
        }
        let mut fds = [
            libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: m, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: dg_fd, events: libc::POLLIN, revents: 0 },
        ];
        // SAFETY: fds is a valid array of 3 pollfds.
        unsafe { libc::poll(fds.as_mut_ptr(), 3, 500) };
        if fds[0].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            // SAFETY: buf is valid for its length.
            let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                let _ = child.kill(); // our terminal window went away
                break 'outer;
            }
            write_all(m, &buf[..n as usize]);
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
        }
    }
    if let Some(t) = orig {
        // SAFETY: restoring the settings saved above.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) };
    }
    let _ = std::fs::remove_file(sock);
    drop(master);
    Ok(child.wait()?.code().unwrap_or(1))
}
