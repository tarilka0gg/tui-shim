//! Native libadwaita pickers (4:3 landscape window, 640x480). Compiled with the `gui` feature.
use crate::palette::Palette;
use crate::picker::Item;
use anyhow::Result;
use libadwaita as adw;
use adw::gtk::{self, gdk, gio, glib};
use adw::prelude::*;
use crate::files::{self, Entry, Kind};
use crate::track::Msg;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

/// Same app-id as the terminal helper window, so one niri window-rule matches both.
const APP_ID: &str = "tui-shim.picker";
const WIDTH: i32 = 640;
const HEIGHT: i32 = 480;

fn css(p: &Palette) -> String {
    let c = |k: &str, d: &str| p.get(k).unwrap_or(d).to_string();
    let (bg, alt, fg) = (c("bg", "#BCC6B7"), c("bg_alt", "#DAD4C1"), c("fg", "#3B342B"));
    let (accent, paper) = (c("accent", "#BD5636"), c("bg_paper", "#F2EDD8"));
    format!(
        "@define-color window_bg_color {bg}; @define-color window_fg_color {fg};\
         @define-color view_bg_color {alt}; @define-color view_fg_color {fg};\
         @define-color headerbar_bg_color {alt}; @define-color headerbar_fg_color {fg};\
         @define-color card_bg_color {alt}; @define-color card_fg_color {fg};\
         @define-color accent_bg_color {accent}; @define-color accent_color {accent};\
         @define-color accent_fg_color {paper};"
    )
}

fn launch(pal: Option<Palette>, build: impl Fn(&adw::Application) + 'static) {
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(move |app| {
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceLight);
        if let (Some(p), Some(display)) = (&pal, gdk::Display::default()) {
            let prov = gtk::CssProvider::new();
            prov.load_from_string(&css(p));
            gtk::style_context_add_provider_for_display(&display, &prov, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        }
        build(app);
    });
    app.run_with_args::<&str>(&[]);
}

fn window(app: &adw::Application, title: &str, content: &impl IsA<gtk::Widget>) -> adw::ApplicationWindow {
    let header = adw::HeaderBar::builder().title_widget(&adw::WindowTitle::new(title, "")).build();
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(content));
    adw::ApplicationWindow::builder()
        .application(app)
        .title(title)
        .default_width(WIDTH)
        .default_height(HEIGHT)
        .content(&view)
        .build()
}

fn niri(args: &[&str]) -> Option<std::process::Output> {
    use std::process::{Command, Stdio};
    Command::new("niri").arg("msg").args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).output().ok()
}

fn place(id: &str, float: bool) {
    if float {
        niri(&["action", "move-window-to-floating", "--id", id]);
    }
    niri(&["action", "set-window-width", "--id", id, &WIDTH.to_string()]);
    niri(&["action", "set-window-height", "--id", id, &HEIGHT.to_string()]);
    niri(&["action", "center-window", "--id", id]);
}

/// Float, size (4:3) and centre our own window through niri IPC, so no window-rule is needed.
/// Silently does nothing outside niri. Runs on its own thread: the window must keep answering
/// the compositor's resize requests while we wait for niri, or the new size never lands.
/// Keeps checking and re-applying until niri reports the target state.
fn float_self() {
    use std::time::Duration;
    if std::env::var_os("NIRI_SOCKET").is_none() {
        return;
    }
    let pid = std::process::id() as u64;
    std::thread::spawn(move || {
        let mut placed = 0;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(300));
            let win = niri(&["-j", "windows"])
                .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok())
                .and_then(|v| v.as_array()?.iter().find(|w| w["pid"].as_u64() == Some(pid) && w["app_id"] == APP_ID).cloned());
            let Some(w) = win else { continue };
            let id = w["id"].as_u64().unwrap_or(0).to_string();
            let floating = w["is_floating"].as_bool().unwrap_or(false);
            let size = &w["layout"]["window_size"];
            if floating && size[0].as_i64() == Some(WIDTH as i64) && size[1].as_i64() == Some(HEIGHT as i64) {
                niri(&["action", "center-window", "--id", &id]); // size is final now
                return;
            }
            if placed < 12 {
                placed += 1;
                place(&id, !floating);
            }
        }
    });
}

fn esc_closes(win: &adw::ApplicationWindow) {
    let kc = gtk::EventControllerKey::new();
    let w = win.clone();
    kc.connect_key_pressed(move |_, key, _, _| {
        if key == gdk::Key::Escape {
            w.close();
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    win.add_controller(kc);
}

/// Index of the chosen item, None if the window was closed.
pub fn pick(title: &str, items: &[Item], start: usize, pal: Option<&Palette>) -> Result<Option<usize>> {
    let result: Rc<Cell<Option<usize>>> = Rc::new(Cell::new(None));
    let rows: Rc<Vec<(String, String, Option<std::path::PathBuf>)>> =
        Rc::new(items.iter().map(|i| (i.label.clone(), i.detail.clone(), i.icon.clone())).collect());
    let title = title.to_string();
    let res = result.clone();
    launch(pal.cloned(), move |app| {
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .css_classes(["boxed-list"])
            .valign(gtk::Align::Start)
            .hexpand(true)
            .build();
        for (i, (label, detail, icon)) in rows.iter().enumerate() {
            let row = adw::ActionRow::builder()
                .title(label)
                .subtitle(detail)
                .use_markup(false)
                .subtitle_lines(1)
                .activatable(true)
                .build();
            let img = match icon {
                Some(p) => gtk::Image::from_file(p),
                None => gtk::Image::from_icon_name("application-x-executable"),
            };
            img.set_pixel_size(40);
            row.add_prefix(&img);
            if i < 9 {
                row.add_suffix(&gtk::Label::builder().label((i + 1).to_string()).css_classes(["dim-label", "numeric"]).build());
            }
            list.append(&row);
        }
        let holder = gtk::Box::builder().hexpand(true).margin_top(12).margin_bottom(12).margin_start(12).margin_end(12).build();
        holder.append(&list);
        let scroller = gtk::ScrolledWindow::builder().child(&holder).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build();
        let win = window(app, &title, &scroller);

        let (w, r) = (win.clone(), res.clone());
        list.connect_row_activated(move |_, row| {
            r.set(Some(row.index() as usize));
            w.close();
        });
        let kc = gtk::EventControllerKey::new();
        let (w, r, n) = (win.clone(), res.clone(), rows.len());
        kc.connect_key_pressed(move |_, key, _, _| {
            match key.to_unicode() {
                Some(c @ '1'..='9') if (c as usize - '1' as usize) < n => {
                    r.set(Some(c as usize - '1' as usize));
                    w.close();
                    glib::Propagation::Stop
                }
                _ if key == gdk::Key::Escape => {
                    w.close();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        win.add_controller(kc);

        win.present();
        float_self();
        if let Some(row) = list.row_at_index(start.min(rows.len().saturating_sub(1)) as i32) {
            list.select_row(Some(&row));
            row.grab_focus();
        }
    });
    Ok(result.get())
}

/// One-line entry. None if the window was closed without Enter.
pub fn prompt(title: &str, field: &str, pal: Option<&Palette>) -> Result<Option<String>> {
    let result: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let (title, field) = (title.to_string(), field.to_string());
    let res = result.clone();
    launch(pal.cloned(), move |app| {
        let entry = adw::EntryRow::builder().title(&field).build();
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .valign(gtk::Align::Start)
            .hexpand(true)
            .margin_top(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        list.append(&entry);
        let win = window(app, &title, &list);
        esc_closes(&win);
        let (w, r) = (win.clone(), res.clone());
        entry.connect_entry_activated(move |e| {
            let t = e.text().trim().to_string();
            if !t.is_empty() {
                *r.borrow_mut() = Some(t);
                w.close();
            }
        });
        win.present();
        float_self();
        entry.grab_focus();
    });
    let out = result.borrow().clone();
    Ok(out)
}

// ---------------------------------------------------------------- file picker

type PickFn = Rc<dyn Fn(PathBuf)>;

const COLS: i32 = 4;

struct Nav {
    dir: PathBuf,
    back: Vec<PathBuf>,
    fwd: Vec<PathBuf>,
    hidden: bool,
    recent: bool,
}

fn tile(e: &Entry, idx: usize) -> gtk::Box {
    let img = match e.kind {
        Kind::Dir => gtk::Image::from_icon_name("folder"),
        Kind::New => gtk::Image::from_icon_name("document-new"),
        _ => {
            let (ct, _) = gio::content_type_guess(Some(&e.label), None);
            gtk::Image::from_gicon(&gio::content_type_get_icon(&ct))
        }
    };
    img.set_pixel_size(48);
    let text = if e.kind == Kind::New { format!("New: {}", e.label) } else { e.label.clone() };
    let label = gtk::Label::builder()
        .label(text)
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .max_width_chars(12)
        .lines(2)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .justify(gtk::Justification::Center)
        .build();
    let b = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(4)
        .margin_end(4)
        .width_request(96)
        .css_classes(["tui-tile", &format!("tui-d{}", (idx / COLS as usize).min(5))])
        .tooltip_text(if e.detail.is_empty() { e.label.clone() } else { format!("{}\n{}", e.label, e.detail) })
        .build();
    b.append(&img);
    b.append(&label);
    b
}

#[derive(Clone, Copy, PartialEq)]
enum Slide {
    /// plain swap (typing in the filter, first fill)
    None,
    /// into a folder / history forward: the new view slides in from the right
    Forward,
    /// up / history back: the new view slides in from the left
    Back,
}

/// Two grid pages in a stack: a listing is built on the hidden page, then slid in.
struct Grid {
    stack: gtk::Stack,
    flows: [gtk::FlowBox; 2],
    scrollers: [gtk::ScrolledWindow; 2],
    active: Cell<usize>,
}

fn make_flow() -> gtk::FlowBox {
    gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .homogeneous(true)
        .min_children_per_line(COLS as u32)
        .max_children_per_line(COLS as u32)
        .activate_on_single_click(true)
        .valign(gtk::Align::Start)
        .build()
}

impl Grid {
    fn new() -> Rc<Grid> {
        let flows = [make_flow(), make_flow()];
        let mk = |f: &gtk::FlowBox| gtk::ScrolledWindow::builder().child(f).vexpand(true).min_content_height(300).hscrollbar_policy(gtk::PolicyType::Never).build();
        let scrollers = [mk(&flows[0]), mk(&flows[1])];
        let stack = gtk::Stack::builder().hhomogeneous(true).vhomogeneous(true).transition_duration(240).vexpand(true).build();
        if std::env::var_os("TUI_SHIM_DEBUG").is_some() {
            let t0 = Instant::now();
            stack.connect_transition_running_notify(move |s| {
                eprintln!("slide: running={} at {:?} type={:?}", s.is_transition_running(), t0.elapsed(), s.transition_type());
            });
        }
        stack.add_named(&scrollers[0], Some("a"));
        stack.add_named(&scrollers[1], Some("b"));
        Rc::new(Grid { stack, flows, scrollers, active: Cell::new(0) })
    }
    fn flow(&self) -> &gtk::FlowBox {
        &self.flows[self.active.get()]
    }
    fn scroller(&self) -> &gtk::ScrolledWindow {
        &self.scrollers[self.active.get()]
    }
    /// the page that is not on screen
    fn hidden_flow(&self) -> &gtk::FlowBox {
        &self.flows[1 - self.active.get()]
    }
    fn flip(&self, slide: Slide) {
        self.stack.set_transition_type(match slide {
            Slide::None => gtk::StackTransitionType::None,
            Slide::Forward => gtk::StackTransitionType::SlideLeft,
            Slide::Back => gtk::StackTransitionType::SlideRight,
        });
        let next = 1 - self.active.get();
        self.stack.set_visible_child(&self.scrollers[next]);
        self.active.set(next);
    }
}

fn flat_button(icon: &str, tip: &str) -> gtk::Button {
    gtk::Button::builder().icon_name(icon).css_classes(["flat"]).tooltip_text(tip).build()
}

fn focus_flow(fb: &gtk::FlowBox) {
    if let Some(c) = fb.selected_children().first().cloned().or_else(|| fb.child_at_index(0)) {
        c.grab_focus();
    } else {
        fb.grab_focus();
    }
}

fn focus_in_entry(w: &impl IsA<gtk::Widget>) -> bool {
    let mut cur = w.root().and_then(|r| r.focus());
    while let Some(x) = cur {
        if x.is::<gtk::Entry>() || x.is::<gtk::SearchEntry>() {
            return true;
        }
        cur = x.parent();
    }
    false
}

/// Thunar-style file browser: nav buttons + path bar on top, search, and a grid of icon tiles.
/// Returns the widget, the widget to focus when it becomes visible, and a refresh hook.
fn file_panel(home: PathBuf, start: PathBuf, reveal: Rc<RefCell<Option<PathBuf>>>, on_pick: PickFn, on_cancel: Rc<dyn Fn()>) -> (gtk::Box, Rc<dyn Fn()>, Rc<dyn Fn()>) {
    let nav = Rc::new(RefCell::new(Nav { dir: start, back: vec![], fwd: vec![], hidden: false, recent: false }));
    let entries: Rc<RefCell<Vec<Entry>>> = Rc::new(RefCell::new(Vec::new()));

    let back_b = flat_button("go-previous-symbolic", "Back (Alt+Left)");
    let fwd_b = flat_button("go-next-symbolic", "Forward (Alt+Right)");
    let up_b = flat_button("go-up-symbolic", "Up (Alt+Up)");
    let home_b = flat_button("user-home-symbolic", "Home (Alt+Home)");
    let recent_b = gtk::ToggleButton::builder().icon_name("document-open-recent-symbolic").css_classes(["flat"]).tooltip_text("Recent files").build();
    let search_b = gtk::ToggleButton::builder().icon_name("system-search-symbolic").css_classes(["flat"]).tooltip_text("Filter (Ctrl+F, or just type)").build();
    let path = gtk::Entry::builder().hexpand(true).build();
    path.set_icon_from_icon_name(gtk::EntryIconPosition::Primary, Some("user-home-symbolic"));
    let filter = gtk::SearchEntry::builder().placeholder_text("Filter - or type a new file name").visible(false).build();
    let grid = Grid::new();

    let bar = gtk::Box::builder().spacing(2).build();
    for b in [&back_b, &fwd_b, &up_b, &home_b] {
        bar.append(b);
    }
    bar.append(&path);
    bar.append(&recent_b);
    bar.append(&search_b);
    let root = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).margin_top(10).margin_bottom(10).margin_start(10).margin_end(10).build();
    root.append(&bar);
    root.append(&filter);
    root.append(&grid.stack);

    let refresh: Rc<dyn Fn(Slide)> = {
        let (nav, entries, grid, filter, path, back_b, fwd_b, up_b, home) = (nav.clone(), entries.clone(), grid.clone(), filter.clone(), path.clone(), back_b.clone(), fwd_b.clone(), up_b.clone(), home.clone());
        let reveal = reveal.clone();
        Rc::new(move |slide: Slide| {
            let flow = grid.hidden_flow().clone();
            flow.remove_all();
            // jump to the folder of the file opened last time and put it in view
            let target = reveal.borrow_mut().take();
            if let Some(parent) = target.as_ref().and_then(|t| t.parent()) {
                let mut n = nav.borrow_mut();
                n.dir = parent.to_path_buf();
                n.recent = false;
            }
            let n = nav.borrow();
            let found = files::view(&home, &n.dir, n.hidden, n.recent, &filter.text());
            let mut flow_idx = 0usize;
            for e in &found {
                flow.append(&tile(e, flow_idx));
                flow_idx += 1;
            }
            *entries.borrow_mut() = found;
            let mut shown = n.dir.display().to_string();
            if !shown.ends_with('/') {
                shown.push('/');
            }
            path.set_text(&shown);
            path.remove_css_class("error");
            back_b.set_sensitive(!n.back.is_empty());
            fwd_b.set_sensitive(!n.fwd.is_empty());
            up_b.set_sensitive(n.dir.parent().is_some());
            let at = target.as_ref().and_then(|t| entries.borrow().iter().position(|e| &e.path == t));
            let chosen = flow.child_at_index(at.unwrap_or(0) as i32);
            if let Some(c) = &chosen {
                flow.select_child(c);
            }
            drop(n);
            grid.flip(slide);
            if let (Some(c), true) = (chosen, at.is_some()) {
                // wait for the layout (and the panel's slide-in) before centring the tile
                let grid = grid.clone();
                glib::timeout_add_local_once(Duration::from_millis(280), move || {
                    if let Some(b) = c.compute_bounds(grid.flow()) {
                        let adj = grid.scroller().vadjustment();
                        adj.set_value(b.y() as f64 - (adj.page_size() - b.height() as f64) / 2.0);
                    }
                });
            }
        })
    };
    let go: Rc<dyn Fn(PathBuf, Slide)> = {
        let (nav, refresh, filter) = (nav.clone(), refresh.clone(), filter.clone());
        Rc::new(move |to, slide| {
            if !to.is_dir() {
                return;
            }
            {
                let mut n = nav.borrow_mut();
                let cur = n.dir.clone();
                if cur != to {
                    n.back.push(cur);
                    n.fwd.clear();
                }
                n.dir = to;
                n.recent = false;
            }
            filter.set_text("");
            refresh(slide);
        })
    };
    let go_back: Rc<dyn Fn()> = {
        let (nav, refresh) = (nav.clone(), refresh.clone());
        Rc::new(move || {
            {
                let mut n = nav.borrow_mut();
                let Some(prev) = n.back.pop() else { return };
                let cur = std::mem::replace(&mut n.dir, prev);
                n.fwd.push(cur);
            }
            refresh(Slide::Back);
        })
    };
    let go_fwd: Rc<dyn Fn()> = {
        let (nav, refresh) = (nav.clone(), refresh.clone());
        Rc::new(move || {
            {
                let mut n = nav.borrow_mut();
                let Some(next) = n.fwd.pop() else { return };
                let cur = std::mem::replace(&mut n.dir, next);
                n.back.push(cur);
            }
            refresh(Slide::Forward);
        })
    };
    let go_up: Rc<dyn Fn()> = {
        let (nav, go) = (nav.clone(), go.clone());
        Rc::new(move || {
            let parent = nav.borrow().dir.parent().map(|p| p.to_path_buf());
            if let Some(p) = parent {
                go(p, Slide::Back);
            }
        })
    };
    let activate: Rc<dyn Fn(usize)> = {
        let (entries, go, on_pick) = (entries.clone(), go.clone(), on_pick.clone());
        Rc::new(move |i| {
            let Some(e) = entries.borrow().get(i).cloned() else { return };
            match e.kind {
                Kind::Dir => go(e.path, Slide::Forward),
                _ => on_pick(e.path),
            }
        })
    };

    back_b.connect_clicked({
        let g = go_back.clone();
        move |_| g()
    });
    fwd_b.connect_clicked({
        let g = go_fwd.clone();
        move |_| g()
    });
    up_b.connect_clicked({
        let g = go_up.clone();
        move |_| g()
    });
    home_b.connect_clicked({
        let (g, h) = (go.clone(), home.clone());
        move |_| g(h.clone(), Slide::Forward)
    });
    recent_b.connect_toggled({
        let (nav, refresh) = (nav.clone(), refresh.clone());
        move |b| {
            nav.borrow_mut().recent = b.is_active();
            refresh(if b.is_active() { Slide::Forward } else { Slide::Back });
        }
    });
    search_b.connect_toggled({
        let (filter, grid) = (filter.clone(), grid.clone());
        move |b| {
            filter.set_visible(b.is_active());
            if b.is_active() {
                filter.grab_focus();
            } else {
                filter.set_text("");
                if let Some(c) = grid.flow().child_at_index(0) {
                    c.grab_focus();
                }
            }
        }
    });
    filter.connect_search_changed({
        let r = refresh.clone();
        move |_| r(Slide::None)
    });
    filter.connect_activate({
        let (a, grid) = (activate.clone(), grid.clone());
        move |_| a(grid.flow().selected_children().first().map(|c| c.index() as usize).unwrap_or(0))
    });
    path.connect_changed(|p| p.remove_css_class("error"));
    path.connect_activate({
        let (home, nav, go, on_pick) = (home.clone(), nav.clone(), go.clone(), on_pick.clone());
        move |p| {
            let t = p.text().trim().to_string();
            let target = if let Some(r) = t.strip_prefix("~/") {
                home.join(r)
            } else if t == "~" {
                home.clone()
            } else if t.starts_with('/') {
                PathBuf::from(&t)
            } else {
                nav.borrow().dir.join(&t)
            };
            if target.is_dir() {
                go(target, Slide::Forward);
            } else if target.parent().is_some_and(|d| d.is_dir()) {
                on_pick(target);
            } else {
                p.add_css_class("error");
            }
        }
    });
    for f in &grid.flows {
        f.connect_child_activated({
            let a = activate.clone();
            move |_, c| a(c.index() as usize)
        });
    }

    // keyboard: Thunar-like shortcuts, arrows from the filter box, type-to-filter from the grid
    let kc = gtk::EventControllerKey::new();
    kc.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let (root2, grid, filter, search_b, path, nav, refresh, entries) =
            (root.clone(), grid.clone(), filter.clone(), search_b.clone(), path.clone(), nav.clone(), refresh.clone(), entries.clone());
        let (g_back, g_fwd, g_up, g_home) = (go_back.clone(), go_fwd.clone(), go_up.clone(), { let (g, h) = (go.clone(), home.clone()); Rc::new(move || g(h.clone(), Slide::Forward)) });
        kc.connect_key_pressed(move |_, key, _, state| {
            let alt = state.contains(gdk::ModifierType::ALT_MASK);
            let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
            let in_entry = focus_in_entry(&root2);
            let select_by = |delta: i32| {
                let (flow, scroller) = (grid.flow(), grid.scroller());
                let cur = flow.selected_children().first().map(|c| c.index()).unwrap_or(-1);
                let n = entries.borrow().len() as i32;
                if n == 0 {
                    return;
                }
                if let Some(c) = flow.child_at_index((cur + delta).clamp(0, n - 1)) {
                    flow.select_child(&c);
                    if let Some(b) = c.compute_bounds(flow) {
                        let adj = scroller.vadjustment();
                        let (top, bottom) = (b.y() as f64, (b.y() + b.height()) as f64);
                        if top < adj.value() {
                            adj.set_value(top);
                        } else if bottom > adj.value() + adj.page_size() {
                            adj.set_value(bottom - adj.page_size());
                        }
                    }
                }
            };
            match key {
                gdk::Key::Escape => {
                    if filter.is_visible() {
                        search_b.set_active(false);
                    } else {
                        on_cancel();
                    }
                }
                gdk::Key::Left if alt => g_back(),
                gdk::Key::Right if alt => g_fwd(),
                gdk::Key::Up if alt => g_up(),
                gdk::Key::Home if alt => g_home(),
                gdk::Key::l | gdk::Key::L if ctrl => {
                    path.grab_focus();
                    path.select_region(0, -1);
                }
                gdk::Key::f | gdk::Key::F if ctrl => search_b.set_active(true),
                gdk::Key::h | gdk::Key::H if ctrl => {
                    let h = nav.borrow().hidden;
                    nav.borrow_mut().hidden = !h;
                    refresh(Slide::None);
                }
                gdk::Key::Down if in_entry => select_by(COLS),
                gdk::Key::Up if in_entry => select_by(-COLS),
                _ => {
                    // typing while the grid has focus starts a filter
                    if !in_entry && !ctrl && !alt {
                        if let Some(c) = key.to_unicode().filter(|c| !c.is_control() && *c != ' ') {
                            search_b.set_active(true);
                            filter.set_text(&c.to_string());
                            filter.set_position(-1);
                            return glib::Propagation::Stop;
                        }
                    }
                    return glib::Propagation::Proceed;
                }
            }
            glib::Propagation::Stop
        });
    }
    root.add_controller(kc);
    let focus: Rc<dyn Fn()> = {
        let grid = grid.clone();
        Rc::new(move || focus_flow(grid.flow()))
    };
    let refresh_plain: Rc<dyn Fn()> = {
        let (r, a, gb, entries) = (refresh.clone(), activate.clone(), go_back.clone(), entries.clone());
        let demo_started = Cell::new(false);
        Rc::new(move || {
            r(Slide::None);
            // debugging aid: after the first fill, walk into a folder and back so the slide can be captured
            if std::env::var_os("TUI_SHIM_DEMO_NAV").is_some() && !demo_started.replace(true) {
                let (a, entries) = (a.clone(), entries.clone());
                glib::timeout_add_local_once(Duration::from_millis(9000), move || {
                    let i = entries.borrow().iter().position(|e| e.kind == Kind::Dir);
                    if let Some(i) = i {
                        a(i);
                    }
                });
                let gb = gb.clone();
                glib::timeout_add_local_once(Duration::from_millis(18000), move || gb());
            }
        })
    };
    (root, focus, refresh_plain)
}

// -------------------------------------------------------------------- overlay

pub struct OverlayCfg {
    pub home: PathBuf,
    pub start: PathBuf,
    pub pal: Option<Palette>,
    pub on_pick: PickFn,
    /// open the panel as soon as the overlay first shows (editors with no file to start from)
    pub expand_on_start: bool,
}

fn monitor_by_name(name: &str) -> Option<gdk::Monitor> {
    let mons = gdk::Display::default()?.monitors();
    (0..mons.n_items()).filter_map(|i| mons.item(i)?.downcast::<gdk::Monitor>().ok()).find(|m| m.connector().as_deref() == Some(name))
}

/// Logical size of the monitor `name` (or the first one): the full-screen surface is that size.
fn screen_size(name: Option<&str>) -> (f64, f64) {
    let mon = name.and_then(monitor_by_name).or_else(|| gdk::Display::default()?.monitors().item(0)?.downcast::<gdk::Monitor>().ok());
    mon.map(|m| (m.geometry().width() as f64, m.geometry().height() as f64)).unwrap_or((1280.0, 800.0))
}

/// Folder button pinned (via layer-shell) to the top-right of the tracked editor window.
/// It expands into the file panel in place and goes away with the window.
const OVERLAY_CSS: &str = r#"
window.tui-overlay { background: transparent; box-shadow: none; }
.tui-root { transition: opacity 200ms ease-in-out; }
.tui-root.tui-away { opacity: 0; }
.tui-fab { background: @card_bg_color; color: @window_fg_color; border: 1px solid alpha(@window_fg_color, 0.25); border-radius: 999px; min-width: 24px; min-height: 24px; padding: 3px; opacity: 0.85; box-shadow: none; outline: none; transition: opacity 160ms, border-color 160ms; }
.tui-fab:hover { opacity: 1; border-color: @accent_bg_color; }
.tui-fab:focus-visible { outline: none; }
.tui-panel { background: @window_bg_color; color: @window_fg_color; border: 1px solid alpha(@window_fg_color, 0.25); border-radius: 14px; box-shadow: 0 2px 14px rgba(0,0,0,.35); }
.tui-panel label, .tui-panel entry, .tui-panel text, .tui-panel button { color: @window_fg_color; }
@keyframes tui-pop { from { opacity: 0; } to { opacity: 1; } }
.tui-tile { animation: tui-pop 220ms ease-out; animation-fill-mode: backwards; }
.tui-d1 { animation-delay: 30ms; } .tui-d2 { animation-delay: 60ms; } .tui-d3 { animation-delay: 90ms; }
.tui-d4 { animation-delay: 120ms; } .tui-d5 { animation-delay: 150ms; }
flowboxchild { transition: background 120ms; border-radius: 10px; }
"#;

pub fn overlay(cfg: OverlayCfg, rx: Receiver<Msg>) {
    let OverlayCfg { home, start, pal, on_pick, expand_on_start } = cfg;
    let rx = Rc::new(rx);
    launch(pal, move |app| {
        let (rx, on_pick) = (rx.clone(), on_pick.clone());
        if let Some(display) = gdk::Display::default() {
            let prov = gtk::CssProvider::new();
            prov.load_from_string(OVERLAY_CSS);
            gtk::style_context_add_provider_for_display(&display, &prov, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1);
        }
        let win = gtk::ApplicationWindow::builder().application(app).decorated(false).resizable(false).css_classes(["tui-overlay"]).build();
        win.init_layer_shell();
        win.set_layer(Layer::Overlay);
        win.set_anchor(Edge::Top, true);
        win.set_anchor(Edge::Right, true);
        // no keyboard focus while collapsed, or the button would steal it from the editor
        win.set_keyboard_mode(KeyboardMode::None);
        win.set_namespace(Some("tui-shim-overlay"));

        // where the user left off: the folder of the file opened last, with that file selected
        let last = files::recent(&home).into_iter().next();
        let start = last.as_ref().and_then(|l| l.parent().map(|p| p.to_path_buf())).unwrap_or_else(|| start.clone());
        let reveal: Rc<RefCell<Option<PathBuf>>> = Rc::new(RefCell::new(last));

        let touched = Rc::new(Cell::new(Instant::now() - Duration::from_secs(60)));
        let rev_slot: Rc<RefCell<Option<gtk::Revealer>>> = Rc::new(RefCell::new(None));
        let collapse: Rc<dyn Fn()> = {
            let (slot, win) = (rev_slot.clone(), win.clone());
            Rc::new(move || {
                if let Some(r) = slot.borrow().as_ref() {
                    r.set_reveal_child(false);
                }
                win.set_keyboard_mode(KeyboardMode::None);
            })
        };
        let (c, t, h, rv) = (collapse.clone(), touched.clone(), home.clone(), reveal.clone());
        let picked: PickFn = Rc::new(move |p| {
            c();
            t.set(Instant::now());
            files::push_recent(&h, &p);
            *rv.borrow_mut() = Some(p.clone()); // next time the panel opens on this file
            on_pick(p);
        });
        let (panel, focus, refresh) = file_panel(home.clone(), start, reveal.clone(), picked, collapse.clone());
        panel.add_css_class("tui-panel");
        panel.set_width_request(540);
        let rev = gtk::Revealer::builder().transition_type(gtk::RevealerTransitionType::SlideDown).transition_duration(220).child(&panel).build();
        *rev_slot.borrow_mut() = Some(rev.clone());

        let toggle = gtk::Button::builder().icon_name("folder-open-symbolic").css_classes(["tui-fab"]).halign(gtk::Align::End).tooltip_text("Open a file").build();
        {
            let (rev, focus, refresh, t, win) = (rev.clone(), focus.clone(), refresh.clone(), touched.clone(), win.clone());
            toggle.connect_clicked(move |_| {
                t.set(Instant::now());
                let show = !rev.reveals_child();
                win.set_keyboard_mode(if show { KeyboardMode::OnDemand } else { KeyboardMode::None });
                if show {
                    refresh();
                }
                rev.set_reveal_child(show);
                if show {
                    focus();
                }
            });
        }
        let root = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).css_classes(["tui-root", "tui-away"]).build();
        // any pointer or key activity on the overlay counts as "in use"
        let motion = gtk::EventControllerMotion::new();
        motion.connect_motion({
            let t = touched.clone();
            move |_, _, _| t.set(Instant::now())
        });
        root.add_controller(motion);
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.connect_key_pressed({
            let t = touched.clone();
            move |_, _, _, _| {
                t.set(Instant::now());
                glib::Propagation::Proceed
            }
        });
        root.add_controller(keys);
        root.append(&toggle);
        root.append(&rev);
        win.set_child(Some(&root));

        let auto_expand = Cell::new(expand_on_start || std::env::var_os("TUI_SHIM_OVERLAY_EXPANDED").is_some());
        let (toggle2, panel) = (toggle.clone(), panel.clone());
        let (w, app2, t) = (win.clone(), app.clone(), touched.clone());
        // set while the editor is not focused; the overlay fades out once it has been idle a moment
        let want_hidden: Rc<Cell<Option<Instant>>> = Rc::new(Cell::new(None));
        glib::timeout_add_local(Duration::from_millis(80), move || {
            while let Ok(m) = rx.try_recv() {
                match m {
                    Msg::Show { right, top, edge_x, monitor, .. } => {
                        want_hidden.set(None);
                        // expand leftwards from the window's right edge; if there is no room
                        // for the panel on that side (narrow window on the left), expand rightwards
                        if edge_x == 0 || edge_x >= 440 {
                            w.set_anchor(Edge::Left, false);
                            w.set_anchor(Edge::Right, true);
                            w.set_margin(Edge::Right, right);
                            toggle2.set_halign(gtk::Align::End);
                            panel.set_width_request(if edge_x == 0 { 540 } else { (edge_x - 12).clamp(440, 540) });
                        } else {
                            w.set_anchor(Edge::Right, false);
                            w.set_anchor(Edge::Left, true);
                            w.set_margin(Edge::Left, (edge_x - 6 - 28).max(0));
                            toggle2.set_halign(gtk::Align::Start);
                            panel.set_width_request(440);
                        }
                        w.set_margin(Edge::Top, top);
                        if let Some(mon) = monitor.as_deref().and_then(monitor_by_name) {
                            w.set_monitor(Some(&mon));
                        }
                        if w.is_visible() {
                            root.remove_css_class("tui-away");
                        } else {
                            root.add_css_class("tui-away");
                            w.present();
                            let r = root.clone();
                            glib::timeout_add_local_once(Duration::from_millis(40), move || r.remove_css_class("tui-away"));
                        }
                        if auto_expand.replace(false) {
                            toggle2.emit_clicked();
                        }
                    }
                    Msg::Click | Msg::Hotkey => {}
                    Msg::Hide => {
                        if want_hidden.get().is_none() {
                            want_hidden.set(Some(Instant::now()));
                        }
                    }
                    Msg::Quit => {
                        app2.quit();
                        return glib::ControlFlow::Break;
                    }
                }
            }
            // clicking the overlay takes focus from the editor, so only hide when it is idle
            if let Some(since) = want_hidden.get() {
                let in_use = w.is_active() || t.get().elapsed() < Duration::from_secs(3);
                if w.is_visible() && !root.has_css_class("tui-away") && since.elapsed() >= Duration::from_millis(600) && !in_use {
                    collapse();
                    root.add_css_class("tui-away"); // fade out, then unmap
                    let (w2, wh) = (w.clone(), want_hidden.clone());
                    glib::timeout_add_local_once(Duration::from_millis(240), move || {
                        if wh.get().is_some() {
                            w2.set_visible(false);
                        }
                    });
                }
            }
            glib::ControlFlow::Continue
        });
        // hold the application open even while the window is hidden
        std::mem::forget(app.hold());
    });
}

/// Size of the area niri leaves for windows on each monitor (monitor minus bars/docks),
/// measured with an invisible surface anchored to all four edges. Returns (connector, w, h).
pub fn usable_areas() -> Vec<(String, i32, i32)> {
    let out: Rc<RefCell<Vec<(String, i32, i32)>>> = Rc::new(RefCell::new(Vec::new()));
    let o = out.clone();
    launch(None, move |app| {
        let Some(display) = gdk::Display::default() else { return };
        let mons = display.monitors();
        let wins: Rc<RefCell<Vec<(String, gtk::ApplicationWindow)>>> = Rc::new(RefCell::new(Vec::new()));
        for i in 0..mons.n_items() {
            let Some(m) = mons.item(i).and_then(|m| m.downcast::<gdk::Monitor>().ok()) else { continue };
            let w = gtk::ApplicationWindow::builder().application(app).decorated(false).css_classes(["tui-overlay"]).build();
            w.init_layer_shell();
            w.set_layer(Layer::Bottom);
            for e in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
                w.set_anchor(e, true);
            }
            w.set_keyboard_mode(KeyboardMode::None);
            w.set_monitor(Some(&m));
            w.set_can_target(false);
            w.present();
            wins.borrow_mut().push((m.connector().map(|c| c.to_string()).unwrap_or_default(), w));
        }
        let (app2, wins2, o2) = (app.clone(), wins.clone(), o.clone());
        glib::timeout_add_local_once(Duration::from_millis(700), move || {
            for (name, w) in wins2.borrow().iter() {
                o2.borrow_mut().push((name.clone(), w.width(), w.height()));
            }
            app2.quit();
        });
    });
    let v = out.borrow().clone();
    v
}

/// The picker as a popup, opened by a click on the badge drawn inside the terminal.
/// No persistent window: the panel is created on a click and removed when it closes.
pub fn panel_overlay(cfg: OverlayCfg, rx: Receiver<Msg>) {
    let OverlayCfg { home, start, pal, on_pick, expand_on_start } = cfg;
    let rx = Rc::new(rx);
    launch(pal, move |app| {
        let (rx, on_pick) = (rx.clone(), on_pick.clone());
        if let Some(display) = gdk::Display::default() {
            let prov = gtk::CssProvider::new();
            prov.load_from_string(OVERLAY_CSS);
            gtk::style_context_add_provider_for_display(&display, &prov, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1);
        }
        let last = files::recent(&home).into_iter().next();
        let start = last.as_ref().and_then(|l| l.parent().map(|p| p.to_path_buf())).unwrap_or_else(|| start.clone());
        let reveal: Rc<RefCell<Option<PathBuf>>> = Rc::new(RefCell::new(last));
        // (right margin, top margin, window right edge x, position known, monitor)
        let geometry: Rc<RefCell<(i32, i32, i32, bool, Option<String>)>> = Rc::new(RefCell::new((10, 10, 0, false, None)));
        let open: Rc<RefCell<Option<gtk::ApplicationWindow>>> = Rc::new(RefCell::new(None));

        // `at_pointer`: opened by a mouse click on the badge, so the pointer marks where the badge is
        let toggle_slot: Rc<RefCell<Option<Rc<dyn Fn(bool)>>>> = Rc::new(RefCell::new(None));
        let opened_at = Rc::new(Cell::new(Instant::now() - Duration::from_secs(60)));
        // how to close the panel that is open right now (animated, and forgets the window)
        let close_slot: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));
        let toggle: Rc<dyn Fn(bool)> = {
            let (app, home, start, reveal, geometry, open, on_pick, toggle_slot, opened_at, close_slot) = (app.clone(), home.clone(), start.clone(), reveal.clone(), geometry.clone(), open.clone(), on_pick.clone(), toggle_slot.clone(), opened_at.clone(), close_slot.clone());
            Rc::new(move |at_pointer: bool| {
                if open.borrow().is_some() {
                    // a second click while the panel is still appearing is the same click, not "close"
                    if opened_at.get().elapsed() > Duration::from_millis(900) {
                        crate::log("badge click while open -> closing");
                        let c = close_slot.borrow().clone();
                        if let Some(c) = c {
                            c();
                        }
                    }
                    return;
                }
                if at_pointer || opened_at.get().elapsed() > Duration::from_millis(900) {
                    opened_at.set(Instant::now());
                }
                // a transparent full-screen surface holds the panel: it can read the pointer position
                // (the compositor will not tell us where a tiled window is) and catches click-away
                let win = gtk::ApplicationWindow::builder().application(&app).decorated(false).resizable(false).css_classes(["tui-overlay"]).build();
                win.init_layer_shell();
                win.set_layer(Layer::Overlay);
                for e in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
                    win.set_anchor(e, true);
                }
                // pointer mode: output coordinates (matches the pointer); otherwise the usable area,
                // which is what the layout estimate is measured in
                win.set_exclusive_zone(if at_pointer { -1 } else { 0 });
                win.set_keyboard_mode(KeyboardMode::OnDemand);
                win.set_namespace(Some("tui-shim-panel"));
                let (right, top, _edge_x, exact, monitor) = geometry.borrow().clone();
                let screen = screen_size(monitor.as_deref());
                crate::log(&format!("open panel: at_pointer={at_pointer} exact={exact} right={right} top={top} monitor={monitor:?} screen={screen:?}"));
                if let Some(mon) = monitor.as_deref().and_then(monitor_by_name) {
                    win.set_monitor(Some(&mon));
                }
                let rev_slot: Rc<RefCell<Option<gtk::Revealer>>> = Rc::new(RefCell::new(None));
                let close: Rc<dyn Fn()> = {
                    let (win, rev_slot, open) = (win.clone(), rev_slot.clone(), open.clone());
                    Rc::new(move || {
                        if open.borrow().is_none() {
                            return;
                        }
                        if let Some(r) = rev_slot.borrow().as_ref() {
                            r.set_reveal_child(false);
                        }
                        open.borrow_mut().take();
                        let win = win.clone();
                        glib::timeout_add_local_once(Duration::from_millis(240), move || win.close());
                    })
                };
                let (c, h, rv, op) = (close.clone(), home.clone(), reveal.clone(), on_pick.clone());
                let picked: PickFn = Rc::new(move |p| {
                    files::push_recent(&h, &p);
                    *rv.borrow_mut() = Some(p.clone()); // next time the panel opens on this file
                    op(p);
                    c();
                });
                let (panel, focus, refresh) = file_panel(home.clone(), start.clone(), reveal.clone(), picked, close.clone());
                panel.add_css_class("tui-panel");
                const PANEL_W: f64 = 540.0;
                panel.set_width_request(PANEL_W as i32);
                let rev = gtk::Revealer::builder().transition_type(gtk::RevealerTransitionType::SlideDown).transition_duration(220).child(&panel).visible(false).build();
                *rev_slot.borrow_mut() = Some(rev.clone());
                let fixed = gtk::Fixed::new();
                fixed.put(&rev, 0.0, 0.0);
                win.set_child(Some(&fixed));
                *open.borrow_mut() = Some(win.clone());
                *close_slot.borrow_mut() = Some(close.clone());
                {
                    // whichever way the window goes away, forget it (but only if it is still the current one)
                    let (open, w) = (open.clone(), win.clone());
                    win.connect_close_request(move |_| {
                        if open.borrow().as_ref() == Some(&w) {
                            open.borrow_mut().take();
                        }
                        glib::Propagation::Proceed
                    });
                }
                {
                    // keyboard focus left the panel (the user went to another window): close it
                    let (close, w) = (close.clone(), win.clone());
                    let seen = Cell::new(false);
                    win.connect_is_active_notify(move |win| {
                        crate::log(&format!("panel is_active={}", win.is_active()));
                        if win.is_active() {
                            seen.set(true);
                        } else if seen.get() {
                            let (close, w) = (close.clone(), w.clone());
                            glib::timeout_add_local_once(Duration::from_millis(250), move || {
                                if !w.is_active() {
                                    crate::log("panel lost focus -> closing");
                                    close();
                                }
                            });
                        }
                    });
                }

                // pointer position inside the full-screen surface = output coordinates
                let pointer: Rc<Cell<Option<(f64, f64)>>> = Rc::new(Cell::new(None));
                // debugging aid: pretend the pointer is at x,y (the nested test session has no pointer)
                if let Some((x, y)) = std::env::var("TUI_SHIM_FAKE_POINTER").ok().and_then(|v| v.split_once(',').and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)))) {
                    pointer.set(Some((x, y)));
                }
                let motion = gtk::EventControllerMotion::new();
                {
                    let p = pointer.clone();
                    motion.connect_enter(move |_, x, y| {
                        crate::log(&format!("pointer enter ({x:.0},{y:.0})"));
                        p.set(Some((x, y)));
                    });
                }
                {
                    let p = pointer.clone();
                    motion.connect_motion(move |_, x, y| {
                        if p.get().is_none() {
                            p.set(Some((x, y)));
                        }
                    });
                }
                win.add_controller(motion);
                // a click outside the panel closes it
                let away = gtk::GestureClick::new();
                away.set_propagation_phase(gtk::PropagationPhase::Capture);
                {
                    let (rev, fixed, close) = (rev.clone(), fixed.clone(), close.clone());
                    away.connect_pressed(move |_, _, x, y| {
                        crate::log(&format!("press on the overlay surface at ({x:.0},{y:.0})"));
                        let inside = rev.compute_bounds(&fixed).is_some_and(|b| b.contains_point(&gtk::graphene::Point::new(x as f32, y as f32)));
                        if !inside {
                            close();
                        }
                    });
                }
                fixed.add_controller(away);

                let remembered = reveal.borrow().clone(); // file to select again if we have to reopen
                refresh();
                win.present();
                // place the panel once the pointer has been seen (mouse click) or the surface is up
                const INSET: f64 = 50.0; // distance from the window's top and right borders
                const MARGIN: f64 = 10.0; // the panel's own margin: its visible edge is this far inside the box
                let (win2, fixed2, rev2, slot2, open2, reveal2) = (win.clone(), fixed.clone(), rev.clone(), toggle_slot.clone(), open.clone(), reveal.clone());
                let began = Instant::now();
                glib::timeout_add_local(Duration::from_millis(60), move || {
                    let (sw, sh) = screen;
                    let place = |x: f64, y: f64| {
                        fixed2.move_(&rev2, x.clamp(8.0, (sw - PANEL_W - 8.0).max(8.0)), y.clamp(8.0, (sh - 440.0).max(8.0)));
                        rev2.set_visible(true);
                        rev2.set_reveal_child(true);
                        focus();
                    };
                    if at_pointer {
                        match pointer.get() {
                            // pointer sits on the badge, which is about 29px in from the window's right
                            // edge and 24px down from its top edge
                            Some((px, py)) => {
                                crate::log(&format!("pointer at ({px:.0},{py:.0}) -> panel placed from the pointer"));
                                place(px + 29.0 - INSET - MARGIN - PANEL_W, py - 24.0 + INSET - MARGIN)
                            }
                            None if began.elapsed() < Duration::from_millis(150) => return glib::ControlFlow::Continue,
                            None => {
                                // the compositor never reported the pointer: use the layout estimate instead
                                crate::log("no pointer event within 150ms -> using the layout estimate");
                                open2.borrow_mut().take();
                                win2.close();
                                *reveal2.borrow_mut() = remembered.clone();
                                if let Some(t) = slot2.borrow().clone() {
                                    t(false);
                                }
                            }
                        }
                    } else if exact {
                        place(sw - (right - 10).max(0) as f64 - INSET - MARGIN - PANEL_W, (top - 10).max(0) as f64 + INSET - MARGIN);
                    } else {
                        place((sw - PANEL_W) / 2.0, 80.0);
                    }
                    glib::ControlFlow::Break
                });
            })
        };

        *toggle_slot.borrow_mut() = Some(toggle.clone());
        let (app2, started) = (app.clone(), Cell::new(!expand_on_start));
        glib::timeout_add_local(Duration::from_millis(80), move || {
            while let Ok(m) = rx.try_recv() {
                match m {
                    Msg::Show { right, top, edge_x, exact, monitor } => {
                        *geometry.borrow_mut() = (right, top, edge_x, exact, monitor);
                        if !started.replace(true) {
                            toggle(false); // an editor with no file yet: offer the picker right away
                        }
                    }
                    Msg::Click => toggle(true),
                    Msg::Hotkey => toggle(false),
                    Msg::Hide => {}
                    Msg::Quit => {
                        app2.quit();
                        return glib::ControlFlow::Break;
                    }
                }
            }
            glib::ControlFlow::Continue
        });
        // no window exists between clicks, so hold the application open
        std::mem::forget(app.hold());
    });
}
