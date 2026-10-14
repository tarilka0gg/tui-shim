mod desktop;
mod icons;
mod palette;
mod registry;
mod scan;
mod theme;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use palette::Palette;
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
    /// Show registry apps and their state
    List,
    /// Create desktop entries, icons and themes for every found app
    Sync {
        /// Overwrite theme files not created by tui-shim
        #[arg(long)]
        force: bool,
        /// Only this app
        app: Option<String>,
    },
    /// Re-apply themes only
    Theme {
        #[arg(long)]
        force: bool,
        app: Option<String>,
    },
    /// Remove desktop entries and icons created by tui-shim
    Clean,
    /// Check the environment
    Doctor,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME unset")?);
    let pal_path = cli
        .palette
        .clone()
        .unwrap_or_else(|| home.join(".config/futuro-darko/palette.md"));
    let apps = registry::load()?;
    let dry = cli.dry_run;

    match &cli.cmd {
        Cmd::Scan { unknown } => {
            for f in scan::scan(&apps) {
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
            for a in &apps {
                let found = scan::which(&a.name).is_some();
                let managed = desktop::apps_dir(&home).join(desktop::file_name(a)).exists();
                println!("{:<10} installed={found:<5} entry={managed}", a.name);
            }
        }
        Cmd::Sync { force, app } => {
            let pal = Palette::from_markdown(&pal_path)?;
            let (term, term_bin) = desktop::detect_terminal().context("no supported terminal found")?;
            let mut n = 0;
            for f in scan::scan(&apps).into_iter().filter(|f| app.as_deref().is_none_or(|a| a == f.app.name)) {
                println!("{}:", f.app.name);
                let icon = icons::resolve(f.app, &home, &pal, dry, cli.offline)?;
                let exec = desktop::exec_line(f.app, &f.bin, term, &term_bin, &pal);
                let path = desktop::apps_dir(&home).join(desktop::file_name(f.app));
                desktop::write(&path, &desktop::render(f.app, &exec, &icon), dry)?;
                theme::apply(f.app, &home, &pal, dry, *force)?;
                n += 1;
            }
            if !dry && n > 0 {
                let _ = std::process::Command::new("update-desktop-database")
                    .arg(desktop::apps_dir(&home))
                    .status();
            }
            println!("{n} app(s) processed");
        }
        Cmd::Theme { force, app } => {
            let pal = Palette::from_markdown(&pal_path)?;
            for f in scan::scan(&apps).into_iter().filter(|f| app.as_deref().is_none_or(|a| a == f.app.name)) {
                println!("{}:", f.app.name);
                theme::apply(f.app, &home, &pal, dry, *force)?;
            }
        }
        Cmd::Clean => {
            let dir = desktop::apps_dir(&home);
            for e in std::fs::read_dir(&dir)?.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "desktop")
                    && std::fs::read_to_string(&p).is_ok_and(|s| s.contains(desktop::MARKER))
                {
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
            println!("palette  {} {}", pal_path.display(), if Palette::from_markdown(&pal_path).is_ok() { "ok" } else { "MISSING/INVALID" });
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
