mod choices;
mod desktop;
mod files;
#[cfg(feature = "gui")]
mod gui;
mod icons;
mod palette;
mod picker;
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
    /// Print a niri window-rule that floats the picker windows
    NiriRule,
    /// Remove desktop entries and icons created by tui-shim
    Clean,
    /// Check the environment
    Doctor,
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
    let start = std::env::current_dir().ok().filter(|d| d.starts_with(&cx.home)).unwrap_or_else(|| cx.home.clone());
    let file = match file {
        Some(f) => Some(f),
        None if app.builtin_picker => None,
        None => match picker::pick_file(&cx.home, &start, th)? {
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
    let (term, term_bin) = desktop::detect_terminal().context("no supported terminal found")?;
    let mut argv = desktop::launch_args(app, &found.bin, term, &term_bin, &cx.pal);
    let sock = (app.remote.as_deref() == Some("nvim")).then(|| {
        let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
        format!("{dir}/tui-shim-nvim-{}.sock", std::process::id())
    });
    if let Some(s) = &sock {
        argv.extend(["--listen".into(), s.clone()]);
    }
    if let Some(f) = &file {
        argv.push(f.to_string_lossy().into_owned());
    }
    let class = format!("tui-shim.{}", app.name);
    #[cfg(feature = "gui")]
    let before = track::window_ids(&class);
    detached(&argv)?;

    #[cfg(feature = "gui")]
    if th.gui && std::env::var_os("NIRI_SOCKET").is_some() {
        let (tx, rx) = std::sync::mpsc::channel();
        track::spawn(class, before, tx);
        let (home, exe, app_name, bin) = (cx.home.clone(), std::env::current_exe()?, app.name.clone(), found.bin.clone());
        let on_pick: std::rc::Rc<dyn Fn(PathBuf)> = std::rc::Rc::new(move |p| {
            let _ = match &sock {
                // nvim: open in the running instance
                Some(s) => detached(&[bin.to_string_lossy().into_owned(), "--server".into(), s.clone(), "--remote-tab".into(), p.to_string_lossy().into_owned()]),
                // others have no remote protocol: a new window (with its own overlay)
                None => detached(&[exe.to_string_lossy().into_owned(), "open".into(), app_name.clone(), p.to_string_lossy().into_owned()]),
            };
        });
        gui::overlay(gui::OverlayCfg { home: home.clone(), start: home, pal: th.palette().cloned(), on_pick }, rx);
    }
    Ok(())
}

fn main() -> Result<()> {
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
