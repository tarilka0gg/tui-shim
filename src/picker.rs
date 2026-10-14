use crate::palette::Palette;
use anyhow::Result;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute, queue,
    style::{Attribute, Color, Print, SetAttribute, SetBackgroundColor, SetForegroundColor},
    terminal::{self, ClearType},
};
use std::io::{self, Write};

pub struct Theme {
    /// show the libadwaita window instead of the terminal UI
    pub gui: bool,
    pal: Option<Palette>,
    bg: Color,
    fg: Color,
    accent: Color,
    sel: Color,
    dim: Color,
}

fn hex(p: &Palette, k: &str) -> Color {
    let Some(h) = p.get(k) else { return Color::Reset };
    let v = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).unwrap_or(0);
    Color::Rgb { r: v(1), g: v(3), b: v(5) }
}

impl Theme {
    pub fn from_palette(p: Option<&Palette>, gui: bool) -> Self {
        let pal = p.cloned();
        match p {
            Some(p) => Self {
                gui,
                pal,
                bg: hex(p, "bg"),
                fg: hex(p, "fg"),
                accent: hex(p, "accent"),
                sel: hex(p, "khaki"),
                dim: hex(p, "fg_dim"),
            },
            None => Self { gui, pal, bg: Color::Reset, fg: Color::Reset, accent: Color::Red, sel: Color::DarkGrey, dim: Color::Grey },
        }
    }
}

pub struct Item {
    pub label: String,
    pub detail: String,
    pub icon: Option<std::path::PathBuf>,
}

struct Guard;
impl Guard {
    fn new() -> Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(io::stdout(), terminal::EnterAlternateScreen, cursor::Hide)?;
        Ok(Self)
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), cursor::Show, terminal::LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

fn clip(s: &str, w: usize) -> String {
    s.chars().take(w).collect()
}

fn frame(out: &mut impl Write, th: &Theme, title: &str) -> Result<()> {
    queue!(
        out,
        SetBackgroundColor(th.bg),
        SetForegroundColor(th.fg),
        terminal::Clear(ClearType::All),
        cursor::MoveTo(2, 1),
        SetForegroundColor(th.accent),
        SetAttribute(Attribute::Bold),
        Print(title),
        SetAttribute(Attribute::NormalIntensity),
    )?;
    Ok(())
}

/// Returns the chosen index, or None when cancelled.
pub fn pick(title: &str, hint: &str, items: &[Item], start: usize, th: &Theme) -> Result<Option<usize>> {
    if items.is_empty() {
        return Ok(None);
    }
    #[cfg(feature = "gui")]
    if th.gui {
        return crate::gui::pick(title, items, start, th.pal.as_ref());
    }
    let _g = Guard::new()?;
    let mut out = io::stdout();
    let mut sel = start.min(items.len() - 1);
    loop {
        let (w, _) = terminal::size()?;
        let w = w as usize;
        frame(&mut out, th, title)?;
        for (i, it) in items.iter().enumerate() {
            let on = i == sel;
            let line = format!("{} {} {}", if on { "▸" } else { " " }, i + 1, it.label);
            queue!(out, cursor::MoveTo(2, 3 + i as u16), SetBackgroundColor(if on { th.sel } else { th.bg }), SetForegroundColor(th.fg))?;
            queue!(out, Print(clip(&format!("{line:<w$}", w = w.saturating_sub(4)), w.saturating_sub(4))))?;
            if on {
                queue!(out, SetBackgroundColor(th.bg), SetForegroundColor(th.dim), cursor::MoveTo(2, 4 + items.len() as u16), Print(clip(&it.detail, w.saturating_sub(4))))?;
            }
        }
        queue!(out, SetBackgroundColor(th.bg), SetForegroundColor(th.dim), cursor::MoveTo(2, 6 + items.len() as u16), Print(clip(hint, w.saturating_sub(4))))?;
        out.flush()?;
        let Event::Key(k) = event::read()? else { continue };
        if k.kind != KeyEventKind::Press {
            continue;
        }
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => sel = (sel + items.len() - 1) % items.len(),
            KeyCode::Down | KeyCode::Char('j') => sel = (sel + 1) % items.len(),
            KeyCode::Enter => return Ok(Some(sel)),
            KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return Ok(None),
            KeyCode::Char(c @ '1'..='9') if (c as usize - '1' as usize) < items.len() => {
                return Ok(Some(c as usize - '1' as usize));
            }
            _ => {}
        }
    }
}

/// One-line text prompt. None when cancelled.
pub fn prompt(title: &str, hint: &str, th: &Theme) -> Result<Option<String>> {
    #[cfg(feature = "gui")]
    if th.gui {
        return crate::gui::prompt(title, "File name (relative paths are under ~)", th.pal.as_ref());
    }
    let _g = Guard::new()?;
    let mut out = io::stdout();
    let mut buf = String::new();
    loop {
        let (w, _) = terminal::size()?;
        frame(&mut out, th, title)?;
        queue!(
            out,
            cursor::MoveTo(2, 3),
            SetForegroundColor(th.fg),
            Print(format!("> {}", clip(&buf, (w as usize).saturating_sub(6)))),
            SetForegroundColor(th.dim),
            cursor::MoveTo(2, 5),
            Print(clip(hint, (w as usize).saturating_sub(4))),
            cursor::MoveTo(4 + buf.chars().count().min(w as usize - 6) as u16, 3),
            cursor::Show,
        )?;
        out.flush()?;
        let Event::Key(k) = event::read()? else { continue };
        if k.kind != KeyEventKind::Press {
            continue;
        }
        match k.code {
            KeyCode::Enter if !buf.trim().is_empty() => return Ok(Some(buf.trim().to_string())),
            KeyCode::Esc => return Ok(None),
            KeyCode::Backspace => {
                buf.pop();
            }
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return Ok(None),
            KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => buf.clear(),
            KeyCode::Char(c) => buf.push(c),
            _ => {}
        }
    }
}
