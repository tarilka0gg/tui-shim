mod choices;
mod desktop;
mod icon;
mod files;
#[cfg(feature = "gui")]
mod gui;
mod icons;
mod palette;
mod picker;
mod probe;
mod pty;
mod track;
mod registry;
mod scan;
mod theme;

use anyhow::{bail, Context, Result};
use choices::Choices;
use clap::{Parser, Subcommand};
use palette::Palette;
use picker::{Item, Theme};
use registry::App;
use std::io::IsTerminal;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about = "Scan for TUI apps, make desktop entries, icons and themes")]
struct Cli {
    /// Palette file (markdown tables); default ~/.config/futuro-darko/palette.md
    #[arg(long, env = "TUI_SHIM_PALETTE", global = true)]
    palette: Option<PathBuf>,
    /// Show what would change without touching anything
    #[arg(long, global = true)]
    dry_run: bool,
    /// Never use the network (icons fall back to generated glyphs)
    #[arg(long, global = true)]
    offline: bool,
    /// Force the terminal picker even when a display is available
    #[arg(long, global = true)]
    tui: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List known apps found on this machine
    Scan {
        /// Also look for unknown ncurses/newt-linked binaries (slow)
        #[arg(long)]
        unknown: bool,
    },
    /// Show registry apps, their copies and state
    List,
    /// Create desktop entries, icons and themes for every found app
    Sync {
        /// Overwrite theme files not created by tui-shim
        #[arg(long)]
        force: bool,
        /// Launchers and icons only; leave app configs alone
        #[arg(long)]
        no_theme: bool,
        /// Only this app
        app: Option<String>,
    },
    /// Re-apply themes only
    Theme {
        #[arg(long)]
        force: bool,
        app: Option<String>,
    },
    /// Pick which copy (system / user-installed) an app's launcher runs
    Choose { app: Option<String> },
    /// "Open with" picker: choose an editor for FILE, or start a new file
    Edit {
        file: Option<PathBuf>,
        /// Ask for a new file name even when a FILE is given
        #[arg(long)]
        new: bool,
    },
    /// Start an editor: file picker first if it has no browser of its own, folder overlay while it runs
    Open {
        app: String,
        file: Option<PathBuf>,
    },
    /// Make plain `micro` / `nvim` in a terminal go through tui-shim (folder overlay in the same window)
    Shims {
        #[arg(value_enum, default_value = "status")]
        action: ShimAction,
    },
    /// (debug) measure the area niri leaves for windows on each monitor
    #[command(hide = true)]
    Usable,
    /// (internal) run a command on a pty that `open` can type into
    #[command(hide = true)]
    PtyRun {
        /// control socket that receives "click" when the in-terminal badge is clicked
        #[arg(long)]
        ctl: Option<PathBuf>,
        /// "bg,fg,ring" colours of the in-terminal badge
        #[arg(long)]
        badge: Option<String>,
        sock: PathBuf,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 1..)]
        argv: Vec<String>,
    },
    /// Print a niri window-rule that floats the picker windows
    NiriRule,
    /// Remove desktop entries and icons created by tui-shim
    Clean,
    /// Check the environment
    Doctor,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum ShimAction {
    Install,
    Remove,
    Status,
}

struct Ctx {
    home: PathBuf,
    pal: Palette,
    dry: bool,
    offline: bool,
}

fn sync_one(cx: &Ctx, f: &scan::Found, term: desktop::Term, term_bin: &str, no_theme: bool, force: bool) -> Result<()> {
    println!("{}:", f.app.name);
    let icon = icons::resolve(f.app, &cx.home, &cx.pal, cx.dry, cx.offline)?;
    let is_editor = f.app.categories.iter().any(|c| c == "TextEditor");
    let exec = if cfg!(feature = "gui") && is_editor {
        // editors go through `open`: file picker + folder overlay
        let exe = std::env::current_exe()?.to_string_lossy().into_owned();
        desktop::direct_exec(&[exe, "open".into(), f.app.name.clone(), "%f".into()])
    } else {
        desktop::exec_line(f.app, &f.bin, term, term_bin, &cx.pal)
    };
    let path = desktop::apps_dir(&cx.home).join(desktop::file_name(f.app));
    desktop::write(&path, &desktop::render(f.app, &exec, &icon), cx.dry)?;
    if !no_theme {
        theme::apply(f.app, &cx.home, &cx.pal, cx.dry, force)?;
    }
    Ok(())
}

/// The chooser and the "Open with" entry. Both run this very binary.
fn write_helpers(cx: &Ctx, term: desktop::Term, term_bin: &str) -> Result<()> {
    let exe = std::env::current_exe()?.to_string_lossy().into_owned();
    let dir = desktop::apps_dir(&cx.home);
    // With the GUI feature the helpers are plain windows; otherwise they run in a terminal.
    let helper = |title: &str, args: &[&str]| -> String {
        let mut cmd = vec![exe.clone()];
        cmd.extend(args.iter().map(|a| a.to_string()));
        if cfg!(feature = "gui") {
            desktop::direct_exec(&cmd)
        } else {
            desktop::helper_exec(term, term_bin, title, &cx.pal, &cmd)
        }
    };
    let choose = helper("Choose TUI versions", &["choose"]);
    desktop::write(
        &dir.join("tui-shim-choose.desktop"),
        &format!(
            "[Desktop Entry]\nType=Application\nName=TUI app versions\nComment=Choose system or user-installed copy of TUI apps\n\
             Exec={choose}\nIcon=preferences-system\nTerminal=false\nCategories=Settings;\n{}\n",
            desktop::MARKER
        ),
        cx.dry,
    )?;
    let edit = helper("Open with", &["edit", "%f"]);
    let new = helper("New file", &["edit", "--new"]);
    desktop::write(
        &dir.join("tui-shim-edit.desktop"),
        &format!(
            "[Desktop Entry]\nType=Application\nName=Edit in terminal...\nComment=Pick a terminal editor for a file\n\
             Exec={edit}\nIcon=accessories-text-editor\nTerminal=false\nCategories=Utility;TextEditor;\n\
             MimeType=text/plain;text/markdown;text/x-shellscript;application/json;application/toml;application/x-yaml;\n\
             Actions=NewFile;\n{}\n\n[Desktop Action NewFile]\nName=New file...\nExec={new}\n",
            desktop::MARKER
        ),
        cx.dry,
    )?;
    Ok(())
}

fn run_choose(cx: &Ctx, th: &Theme, apps: &[App], only: &Option<String>) -> Result<Vec<String>> {
    let mut ch = Choices::load(&cx.home);
    let mut changed = Vec::new();
    for a in apps.iter().filter(|a| only.as_deref().is_none_or(|n| n == a.name)) {
        let vs = scan::variants(&a.name, &cx.home);
        if vs.len() < 2 {
            if only.is_some() {
                println!("{}: {} copy found, nothing to choose", a.name, vs.len());
            }
            continue;
        }
        let arg = a.version_arg.as_deref().unwrap_or("--version");
        let items: Vec<Item> = vs
            .iter()
            .map(|v| Item {
                label: a.title.clone(),
                detail: format!("{} - {} - {}", v.origin, v.path.display(), scan::version(&v.path, arg)),
                icon: icons::find_installed(&cx.home, &a.icon_names),
            })
            .collect();
        let cur = ch.bin.get(&a.name).and_then(|c| vs.iter().position(|v| v.path.to_str() == Some(c.as_str())));
        let hint = "up/down or 1-9 select - enter confirm - esc skip";
        if let Some(i) = picker::pick(&format!("Which {}?", a.title), hint, &items, cur.unwrap_or(0), th)? {
            ch.bin.insert(a.name.clone(), vs[i].path.to_string_lossy().into_owned());
            changed.push(a.name.clone());
        }
    }
    if changed.is_empty() && only.is_none() {
        println!("no known app has more than one copy installed");
    }
    if !cx.dry && !changed.is_empty() {
        ch.save(&cx.home)?;
    }
    Ok(changed)
}

fn run_edit(cx: &Ctx, th: &Theme, apps: &[App], file: &Option<PathBuf>, new: bool) -> Result<()> {
    let editors: Vec<scan::Found> = scan::scan(apps, &cx.home)
        .into_iter()
        .filter(|f| f.app.categories.iter().any(|c| c == "TextEditor"))
        .collect();
    if editors.is_empty() {
        bail!("no known terminal editor installed");
    }
    let mut ch = Choices::load(&cx.home);
    let last = ch.editor.as_deref().and_then(|l| editors.iter().position(|f| f.app.name == l)).unwrap_or(0);
    let pick = if editors.len() == 1 {
        0
    } else {
        let items: Vec<Item> = editors
            .iter()
            .map(|f| Item {
                label: f.app.title.clone(),
                detail: f.bin.display().to_string(),
                icon: icons::find_installed(&cx.home, &f.app.icon_names),
            })
            .collect();
        let title = match file {
            Some(f) if !new => format!("Open {} with", f.file_name().unwrap_or_default().to_string_lossy()),
            _ => "New file with".to_string(),
        };
        match picker::pick(&title, "up/down or 1-9 select - enter confirm - esc cancel", &items, last, th)? {
            Some(i) => i,
            None => return Ok(()),
        }
    };
    ch.editor = Some(editors[pick].app.name.clone());
    let _ = ch.save(&cx.home);

    let target = match file {
        Some(f) if !new => f.clone(),
        _ => {
            let Some(name) = picker::prompt("New file name", "relative paths are under ~ - enter confirm - esc cancel", th)? else {
                return Ok(());
            };
            let p = theme::expand(&cx.home, &name);
            let p = if p.is_absolute() { p } else { cx.home.join(p) };
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            p
        }
    };
    if std::io::stdin().is_terminal() {
        // already inside a terminal: become the editor
        let err = std::process::Command::new(&editors[pick].bin).arg(&target).exec();
        bail!("cannot start {}: {err}", editors[pick].bin.display());
    }
    // started from a launcher / the GUI picker: the editor needs a terminal window
    let (term, term_bin) = desktop::detect_terminal().context("no supported terminal found")?;
    let f = &editors[pick];
    let mut argv = desktop::launch_args(f.app, &f.bin, term, &term_bin, &cx.pal);
    argv.push(target.to_string_lossy().into_owned());
    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    // SAFETY: setsid only; lets the terminal outlive this process.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().with_context(|| format!("cannot start {term_bin}"))?;
    Ok(())
}

fn detached(argv: &[String]) -> Result<()> {
    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    // SAFETY: setsid only; the new process must outlive this one.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().with_context(|| format!("cannot start {}", argv[0]))?;
    Ok(())
}

fn run_open(cx: &Ctx, th: &Theme, apps: &[App], name: &str, file: Option<PathBuf>) -> Result<()> {
    let app = apps.iter().find(|a| a.name == name).with_context(|| format!("unknown app {name}"))?;
    let found = scan::scan(apps, &cx.home).into_iter().find(|f| f.app.name == name).with_context(|| format!("{name} is not installed"))?;
    let overlay = cfg!(feature = "gui") && th.gui && std::env::var_os("NIRI_SOCKET").is_some();
    let file = match file {
        Some(f) => Some(f),
        // the overlay offers the picker on top of the editor; without it, ask up front
        None if app.builtin_picker || overlay => None,
        None => match picker::pick_file(&cx.home, &cx.home, th)? {
            Some(f) => Some(f),
            None => return Ok(()),
        },
    };
    if let Some(f) = &file {
        if let Some(parent) = f.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        files::push_recent(&cx.home, f);
    }
    // started from a terminal: run in that terminal instead of opening another window
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        return run_here(cx, th, app, &found.bin, file, overlay);
    }
    let (term, term_bin) = desktop::detect_terminal().context("no supported terminal found")?;
    let exe = std::env::current_exe()?;
    // editors we know how to type into run under our pty wrapper so the overlay can open files in place
    let sock = (overlay && app.open_keys.is_some()).then(|| pty::socket_path(std::process::id()));
    let badge = sock.is_some() && matches!(term, desktop::Term::Ghostty | desktop::Term::Kitty);
    let ctl = sock.as_ref().map(|s| s.with_extension("ctl"));
    let mut argv = if sock.is_some() {
        // launch_args ends with the program to run: make that the wrapper
        let mut a = desktop::launch_args(app, &exe, term, &term_bin, &cx.pal);
        a.push("pty-run".into());
        if let (true, Some(c)) = (badge, &ctl) {
            a.extend(["--ctl".into(), c.to_string_lossy().into_owned(), "--badge".into(), badge_colors(th.palette())]);
        }
        a.extend([sock.as_ref().unwrap().to_string_lossy().into_owned(), "--".into(), found.bin.to_string_lossy().into_owned()]);
        a
    } else {
        desktop::launch_args(app, &found.bin, term, &term_bin, &cx.pal)
    };
    if let Some(f) = &file {
        argv.push(f.to_string_lossy().into_owned());
    }
    let class = format!("tui-shim.{}", app.name);
    #[cfg(feature = "gui")]
    let before = track::window_ids(&class);
    detached(&desktop::finish_args(term, argv))?;

    #[cfg(feature = "gui")]
    if overlay {
        let (tx, rx) = std::sync::mpsc::channel();
        track::spawn(class, before, tx.clone());
        if let (true, Some(c)) = (badge, ctl) {
            listen_ctl(c, tx);
        }
        let sock_for_probe = sock.clone();
        let (home, name, app2) = (cx.home.clone(), app.name.clone(), app.clone());
        let on_pick: std::rc::Rc<dyn Fn(PathBuf)> = std::rc::Rc::new(move |p: PathBuf| {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match (&sock, app2.open_keys(&p)) {
                // type the "open" command into the running editor
                (Some(s), Some(keys)) => {
                    let _ = pty::inject(s, &keys);
                }
                // no known way to drive this editor: a new window (with its own overlay)
                _ => {
                    let _ = detached(&[exe.to_string_lossy().into_owned(), "open".into(), name.clone(), p.to_string_lossy().into_owned()]);
                }
            }
        });
        let expand_on_start = file.is_none(); // every editor offers the picker when it starts with no file
        let probe = match (badge, &sock_for_probe) {
            (true, Some(sk)) => Some(gui::ProbeCfg { sock: sk.clone(), fill: icon::marker_fill(pal_rgb(th.palette(), "bg_alt", "#DAD4C1")) }),
            _ => None,
        };
        let cfg = gui::OverlayCfg { probe, home: home.clone(), start: home, pal: th.palette().cloned(), on_pick, expand_on_start };
        if badge {
            gui::panel_overlay(cfg, rx);
        } else {
            gui::overlay(cfg, rx);
        }
    }
    Ok(())
}

/// Append one line to ~/.cache/tui-shim/overlay.log (kept small). For diagnosing the overlay.
pub fn log(line: &str) {
    let Some(home) = std::env::var_os("HOME") else { return };
    let path = PathBuf::from(home).join(".cache/tui-shim/overlay.log");
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 200_000) {
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        use std::io::Write;
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() % 100_000_000).unwrap_or(0);
        let _ = writeln!(f, "[{}.{:03}] pid={} {line}", t / 1000, t % 1000, std::process::id());
    }
}

fn pal_rgb(pal: Option<&Palette>, key: &str, default: &str) -> [u8; 3] {
    icon::parse_hex(pal.and_then(|p| p.get(key)).unwrap_or(default)).unwrap_or([128, 128, 128])
}

/// Colours of the in-terminal badge as "bg,fg,ring" hex.
fn badge_colors(pal: Option<&Palette>) -> String {
    let g = |k: &str, d: &str| pal.and_then(|p| p.get(k)).unwrap_or(d).to_string();
    format!("{},{},{}", g("bg_alt", "#DAD4C1"), g("fg", "#3B342B"), g("secondary", "#7D8278")).replace('#', "")
}

/// Can the terminal we are running in draw images (kitty graphics protocol)?
fn terminal_draws_images() -> bool {
    let var = |k: &str| std::env::var(k).unwrap_or_default();
    matches!(var("TERM_PROGRAM").as_str(), "ghostty" | "kitty") || var("TERM").contains("ghostty") || var("TERM").contains("kitty")
}

/// Forward "click" datagrams from the in-terminal badge to the overlay as messages.
#[cfg(feature = "gui")]
fn listen_ctl(path: PathBuf, tx: std::sync::mpsc::Sender<track::Msg>) {
    let _ = std::fs::remove_file(&path);
    let Ok(sock) = std::os::unix::net::UnixDatagram::bind(&path) else { return };
    std::thread::spawn(move || {
        let mut buf = [0u8; 64];
        while let Ok(n) = sock.recv(&mut buf) {
            let at = std::time::Instant::now(); // when it arrived, not when it gets handled
            let msg = match &buf[..n] {
                b"key" => track::Msg::Hotkey(at),
                b"away" => track::Msg::Away(at),
                _ => track::Msg::Click(at),
            };
            log(&format!("ctl: {msg:?}"));
            if tx.send(msg).is_err() {
                break;
            }
        }
        let _ = std::fs::remove_file(&path);
    });
}

/// The overlay shares this terminal in terminal mode; stray GTK/Mesa warnings on stderr would be
/// drawn over the editor. Send them to a log (TUI_SHIM_LOG) or nowhere.
fn silence_stderr() {
    let path = std::env::var("TUI_SHIM_LOG").unwrap_or_else(|_| "/dev/null".into());
    if let Ok(c) = std::ffi::CString::new(path) {
        // SAFETY: open/dup2/close on a freshly opened fd; fd 2 is replaced, nothing else is touched.
        unsafe {
            let fd = libc::open(c.as_ptr(), libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND, 0o644);
            if fd >= 0 {
                libc::dup2(fd, 2);
                libc::close(fd);
            }
        }
    }
}

/// Run the editor in the current terminal. With the overlay available it runs on our pty
/// (so the overlay can open files in place) and the overlay attaches to this terminal's window.
fn run_here(cx: &Ctx, th: &Theme, app: &App, bin: &std::path::Path, file: Option<PathBuf>, overlay: bool) -> Result<()> {
    let mut argv = vec![bin.to_string_lossy().into_owned()];
    if let Some(f) = &file {
        argv.push(f.to_string_lossy().into_owned());
    }
    #[cfg(feature = "gui")]
    if overlay && app.open_keys.is_some() {
        if let Some(id) = track::own_window_id() {
            let sock = pty::socket_path(std::process::id());
            let ctl = sock.with_extension("ctl");
            // terminals that draw images get the button inside the window, always at its corner
            let badge = terminal_draws_images();
            log(&format!("terminal mode: badge={badge} TERM_PROGRAM={:?} TERM={:?} window={id}", std::env::var("TERM_PROGRAM").ok(), std::env::var("TERM").ok()));
            let deco = badge.then(|| {
                let pal = th.palette();
                pty::Deco { bg: pal_rgb(pal, "bg_alt", "#DAD4C1"), fg: pal_rgb(pal, "fg", "#3B342B"), ring: pal_rgb(pal, "secondary", "#7D8278"), ctl: ctl.clone() }
            });
            let (sock2, argv2) = (sock.clone(), argv.clone());
            silence_stderr();
            let editor = std::thread::spawn(move || {
                let code = pty::run(&sock2, &argv2, deco).unwrap_or(1);
                std::process::exit(code); // editor exited: take the overlay down with it
            });
            let (tx, rx) = std::sync::mpsc::channel();
            track::follow_window(id, tx.clone());
            if badge {
                listen_ctl(ctl, tx);
            }
            let app2 = app.clone();
            let sock_probe = sock.clone();
            let on_pick: std::rc::Rc<dyn Fn(PathBuf)> = std::rc::Rc::new(move |p: PathBuf| {
                if let Some(parent) = p.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Some(keys) = app2.open_keys(&p) {
                    let _ = pty::inject(&sock, &keys);
                }
            });
            let home = cx.home.clone();
            let expand_on_start = file.is_none(); // every editor offers the picker when it starts with no file
            let probe = badge.then(|| gui::ProbeCfg { sock: sock_probe.clone(), fill: icon::marker_fill(pal_rgb(th.palette(), "bg_alt", "#DAD4C1")) });
            let cfg = gui::OverlayCfg { probe, home: home.clone(), start: home, pal: th.palette().cloned(), on_pick, expand_on_start };
            if badge {
                gui::panel_overlay(cfg, rx);
            } else {
                gui::overlay(cfg, rx);
            }
            let _ = editor.join();
            return Ok(());
        }
    }
    let _ = (cx, th, app, overlay);
    let err = std::process::Command::new(&argv[0]).args(&argv[1..]).exec();
    bail!("cannot start {}: {err}", argv[0])
}

/// Invoked through a shim symlink (`~/.local/bin/micro` -> tui-shim): behave like `open micro`
/// for plain interactive use, and hand everything else (flags, several files, pipes) to the real editor.
fn run_shim(name: &str) -> Result<()> {
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME unset")?);
    let apps = registry::load()?;
    let app = apps.iter().find(|a| a.name == name && a.categories.iter().any(|c| c == "TextEditor")).with_context(|| format!("{name}: not a known editor"))?;
    let real = scan::scan(&apps, &home).into_iter().find(|f| f.app.name == name).with_context(|| format!("real {name} not found"))?.bin;
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let plain = args.len() <= 1 && args.iter().all(|a| !a.to_string_lossy().starts_with('-'));
    if !interactive || !plain {
        let err = std::process::Command::new(&real).args(&args).exec();
        bail!("cannot start {}: {err}", real.display());
    }
    let pal_path = home.join(".config/futuro-darko/palette.md");
    let pal = Palette::from_markdown(&pal_path).ok();
    let use_gui = cfg!(feature = "gui") && (std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some());
    let cx = Ctx { home: home.clone(), pal: pal.clone().unwrap_or_else(|| Palette { name: "none".into(), colors: Default::default() }), dry: false, offline: true };
    let file = args.first().map(PathBuf::from);
    let _ = app;
    run_open(&cx, &Theme::from_palette(pal.as_ref(), use_gui), &apps, name, file)
}

fn run_shims(home: &std::path::Path, apps: &[App], action: ShimAction) -> Result<()> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let dir = home.join(".local/bin");
    let path_dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    let pos = |d: &std::path::Path| path_dirs.iter().position(|p| p.canonicalize().ok() == d.canonicalize().ok());
    for a in apps.iter().filter(|a| a.categories.iter().any(|c| c == "TextEditor")) {
        let link = dir.join(&a.name);
        let ours = std::fs::canonicalize(&link).ok().as_ref() == Some(&exe);
        match action {
            ShimAction::Status => {
                let real = scan::variants(&a.name, home).first().map(|v| v.path.display().to_string()).unwrap_or_else(|| "not installed".into());
                println!("{:<8} shim={:<5} real={real}", a.name, ours);
            }
            ShimAction::Install => {
                if scan::variants(&a.name, home).is_empty() {
                    println!("{}: not installed, skipped", a.name);
                } else if ours {
                    println!("{}: shim already in place", a.name);
                } else if link.symlink_metadata().is_ok() {
                    println!("{}: {} exists and is not ours, left alone", a.name, link.display());
                } else {
                    std::fs::create_dir_all(&dir)?;
                    std::os::unix::fs::symlink(&exe, &link)?;
                    println!("{}: {} -> {}", a.name, link.display(), exe.display());
                }
            }
            ShimAction::Remove => {
                if ours {
                    std::fs::remove_file(&link)?;
                    println!("{}: removed {}", a.name, link.display());
                }
            }
        }
    }
    if matches!(action, ShimAction::Install) {
        let real_pos = apps.iter().filter(|a| a.name == "micro").filter_map(|a| scan::variants(&a.name, home).first().and_then(|v| v.path.parent().and_then(|d| pos(d)))).next();
        match (pos(&dir), real_pos) {
            (Some(s), Some(r)) if s >= r => println!("note: {} comes after the real editors in $PATH, so the shims will not take effect until it comes first", dir.display()),
            (None, _) => println!("note: {} is not in $PATH", dir.display()),
            _ => {}
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    // called through a shim symlink (e.g. ~/.local/bin/micro)?
    if let Some(name) = std::env::args_os().next().and_then(|a| PathBuf::from(a).file_name().map(|n| n.to_string_lossy().into_owned())) {
        if !name.starts_with("tui-shim") {
            return run_shim(&name);
        }
    }
    let cli = Cli::parse();
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME unset")?);
    let pal_path = cli.palette.clone().unwrap_or_else(|| home.join(".config/futuro-darko/palette.md"));
    let apps = registry::load()?;
    let dry = cli.dry_run;
    let use_gui = cfg!(feature = "gui")
        && !cli.tui
        && (std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some());
    let load_pal = || Palette::from_markdown(&pal_path);
    let ctx = || -> Result<Ctx> { Ok(Ctx { home: home.clone(), pal: load_pal()?, dry, offline: cli.offline }) };

    match &cli.cmd {
        Cmd::Scan { unknown } => {
            for f in scan::scan(&apps, &home) {
                println!("{:<10} {}", f.app.name, f.bin.display());
            }
            if *unknown {
                println!("\nunmanaged candidates (ncurses/newt-linked, not in registry):");
                for c in scan::candidates(&apps) {
                    println!("  {}", c.display());
                }
            }
        }
        Cmd::List => {
            let ch = Choices::load(&home);
            for a in &apps {
                let vs = scan::variants(&a.name, &home);
                let managed = desktop::apps_dir(&home).join(desktop::file_name(a)).exists();
                let copies: Vec<String> = vs.iter().map(|v| format!("{}:{}", v.origin, v.path.display())).collect();
                let pinned = if ch.bin.contains_key(&a.name) { " (choice saved)" } else { "" };
                println!("{:<10} entry={managed:<5} {}{pinned}", a.name, copies.join("  "));
            }
        }
        Cmd::Sync { force, no_theme, app } => {
            let cx = ctx()?;
            let (term, term_bin) = desktop::detect_terminal().context("no supported terminal found")?;
            let mut n = 0;
            for f in scan::scan(&apps, &home).iter().filter(|f| app.as_deref().is_none_or(|a| a == f.app.name)) {
                sync_one(&cx, f, term, &term_bin, *no_theme, *force)?;
                n += 1;
            }
            if app.is_none() {
                write_helpers(&cx, term, &term_bin)?;
            }
            if !dry && n > 0 {
                let _ = std::process::Command::new("update-desktop-database").arg(desktop::apps_dir(&home)).status();
            }
            println!("{n} app(s) processed");
        }
        Cmd::Theme { force, app } => {
            let cx = ctx()?;
            for f in scan::scan(&apps, &home).iter().filter(|f| app.as_deref().is_none_or(|a| a == f.app.name)) {
                println!("{}:", f.app.name);
                theme::apply(f.app, &cx.home, &cx.pal, dry, *force)?;
            }
        }
        Cmd::Choose { app } => {
            let pal = load_pal().ok();
            let cx = Ctx { home: home.clone(), pal: pal.clone().unwrap_or_else(|| Palette { name: "none".into(), colors: Default::default() }), dry, offline: cli.offline };
            let th = Theme::from_palette(pal.as_ref(), use_gui);
            let changed = run_choose(&cx, &th, &apps, app)?;
            if !changed.is_empty() && pal.is_some() {
                let (term, term_bin) = desktop::detect_terminal().context("no supported terminal found")?;
                for f in scan::scan(&apps, &home).iter().filter(|f| changed.contains(&f.app.name)) {
                    sync_one(&cx, f, term, &term_bin, true, false)?;
                }
            }
        }
        Cmd::Edit { file, new } => {
            let pal = load_pal().ok();
            let cx = Ctx { home: home.clone(), pal: pal.clone().unwrap_or_else(|| Palette { name: "none".into(), colors: Default::default() }), dry, offline: cli.offline };
            run_edit(&cx, &Theme::from_palette(pal.as_ref(), use_gui), &apps, file, *new)?;
        }
        Cmd::Open { app, file } => {
            let pal = load_pal().ok();
            let cx = Ctx { home: home.clone(), pal: pal.clone().unwrap_or_else(|| Palette { name: "none".into(), colors: Default::default() }), dry, offline: cli.offline };
            run_open(&cx, &Theme::from_palette(pal.as_ref(), use_gui), &apps, app, file.clone())?;
        }
        Cmd::Usable => {
            #[cfg(feature = "gui")]
            for (name, w, h) in gui::usable_areas() {
                println!("{name}: usable {w}x{h}");
            }
        }
        Cmd::Shims { action } => run_shims(&home, &apps, *action)?,
        Cmd::PtyRun { ctl, badge, sock, argv } => {
            let deco = ctl.clone().zip(badge.as_deref()).and_then(|(ctl, b)| {
                let c: Vec<[u8; 3]> = b.split(',').filter_map(icon::parse_hex).collect();
                (c.len() == 3).then(|| pty::Deco { bg: c[0], fg: c[1], ring: c[2], ctl })
            });
            std::process::exit(pty::run(sock, argv, deco)?);
        }
        Cmd::NiriRule => {
            println!("// add to ~/.config/niri/config.kdl\nwindow-rule {{\n    match app-id=r#\"^tui-shim\\.picker$\"#\n    open-floating true\n}}");
        }
        Cmd::Clean => {
            let dir = desktop::apps_dir(&home);
            for e in std::fs::read_dir(&dir)?.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "desktop") && std::fs::read_to_string(&p).is_ok_and(|s| s.contains(desktop::MARKER)) {
                    println!("remove {}", p.display());
                    if !dry {
                        std::fs::remove_file(&p)?;
                    }
                }
            }
            for sub in ["scalable", "256x256"] {
                let d = home.join(format!(".local/share/icons/hicolor/{sub}/apps"));
                let Ok(rd) = std::fs::read_dir(&d) else { continue };
                for e in rd.flatten() {
                    if e.file_name().to_string_lossy().starts_with("tui-shim-") {
                        println!("remove {}", e.path().display());
                        if !dry {
                            std::fs::remove_file(e.path())?;
                        }
                    }
                }
            }
        }
        Cmd::Doctor => {
            println!("palette  {} {}", pal_path.display(), if load_pal().is_ok() { "ok" } else { "MISSING/INVALID" });
            match desktop::detect_terminal() {
                Some((_, b)) => println!("terminal {b}"),
                None => println!("terminal NONE (ghostty/alacritty/foot/kitty)"),
            }
            println!("desktop-file-validate {}", if scan::which("desktop-file-validate").is_some() { "ok" } else { "missing" });
            println!("apps dir {}", desktop::apps_dir(&home).display());
        }
    }
    Ok(())
}
