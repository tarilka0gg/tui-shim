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

fn tile(e: &Entry) -> gtk::Box {
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
        .tooltip_text(if e.detail.is_empty() { e.label.clone() } else { format!("{}\n{}", e.label, e.detail) })
        .build();
    b.append(&img);
    b.append(&label);
    b
}

fn flat_button(icon: &str, tip: &str) -> gtk::Button {
    gtk::Button::builder().icon_name(icon).css_classes(["flat"]).tooltip_text(tip).build()
}

fn focus_first(w: &gtk::Widget) {
    if let Some(fb) = w.downcast_ref::<gtk::FlowBox>() {
        if let Some(c) = fb.child_at_index(0) {
            c.grab_focus();
            return;
        }
    }
    w.grab_focus();
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
fn file_panel(home: PathBuf, start: PathBuf, on_pick: PickFn, on_cancel: Rc<dyn Fn()>) -> (gtk::Box, gtk::Widget, Rc<dyn Fn()>) {
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
    let flow = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .homogeneous(true)
        .min_children_per_line(COLS as u32)
        .max_children_per_line(COLS as u32)
        .activate_on_single_click(true)
        .valign(gtk::Align::Start)
        .build();
    let scroller = gtk::ScrolledWindow::builder().child(&flow).vexpand(true).min_content_height(300).hscrollbar_policy(gtk::PolicyType::Never).build();

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
    root.append(&scroller);

    let refresh: Rc<dyn Fn()> = {
        let (nav, entries, flow, filter, path, back_b, fwd_b, up_b, home) = (nav.clone(), entries.clone(), flow.clone(), filter.clone(), path.clone(), back_b.clone(), fwd_b.clone(), up_b.clone(), home.clone());
        Rc::new(move || {
            flow.remove_all();
            let n = nav.borrow();
            let found = files::view(&home, &n.dir, n.hidden, n.recent, &filter.text());
            for e in &found {
                flow.append(&tile(e));
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
            if let Some(c) = flow.child_at_index(0) {
                flow.select_child(&c);
            }
        })
    };
    let go: Rc<dyn Fn(PathBuf)> = {
        let (nav, refresh, filter) = (nav.clone(), refresh.clone(), filter.clone());
        Rc::new(move |to| {
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
            refresh();
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
            refresh();
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
            refresh();
        })
    };
    let go_up: Rc<dyn Fn()> = {
        let (nav, go) = (nav.clone(), go.clone());
        Rc::new(move || {
            let parent = nav.borrow().dir.parent().map(|p| p.to_path_buf());
            if let Some(p) = parent {
                go(p);
            }
        })
    };
    let activate: Rc<dyn Fn(usize)> = {
        let (entries, go, on_pick) = (entries.clone(), go.clone(), on_pick.clone());
        Rc::new(move |i| {
            let Some(e) = entries.borrow().get(i).cloned() else { return };
            match e.kind {
                Kind::Dir => go(e.path),
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
        move |_| g(h.clone())
    });
    recent_b.connect_toggled({
        let (nav, refresh) = (nav.clone(), refresh.clone());
        move |b| {
            nav.borrow_mut().recent = b.is_active();
            refresh();
        }
    });
    search_b.connect_toggled({
        let (filter, flow) = (filter.clone(), flow.clone());
        move |b| {
            filter.set_visible(b.is_active());
            if b.is_active() {
                filter.grab_focus();
            } else {
                filter.set_text("");
                if let Some(c) = flow.child_at_index(0) {
                    c.grab_focus();
                }
            }
        }
    });
    filter.connect_search_changed({
        let r = refresh.clone();
        move |_| r()
    });
    filter.connect_activate({
        let (a, flow) = (activate.clone(), flow.clone());
        move |_| a(flow.selected_children().first().map(|c| c.index() as usize).unwrap_or(0))
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
                go(target);
            } else if target.parent().is_some_and(|d| d.is_dir()) {
                on_pick(target);
            } else {
                p.add_css_class("error");
            }
        }
    });
    flow.connect_child_activated({
        let a = activate.clone();
        move |_, c| a(c.index() as usize)
    });

    // keyboard: Thunar-like shortcuts, arrows from the filter box, type-to-filter from the grid
    let kc = gtk::EventControllerKey::new();
    kc.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let (root2, flow, scroller, filter, search_b, path, nav, refresh, entries) =
            (root.clone(), flow.clone(), scroller.clone(), filter.clone(), search_b.clone(), path.clone(), nav.clone(), refresh.clone(), entries.clone());
        let (g_back, g_fwd, g_up, g_home) = (go_back.clone(), go_fwd.clone(), go_up.clone(), { let (g, h) = (go.clone(), home.clone()); Rc::new(move || g(h.clone())) });
        kc.connect_key_pressed(move |_, key, _, state| {
            let alt = state.contains(gdk::ModifierType::ALT_MASK);
            let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
            let in_entry = focus_in_entry(&root2);
            let select_by = |delta: i32| {
                let cur = flow.selected_children().first().map(|c| c.index()).unwrap_or(-1);
                let n = entries.borrow().len() as i32;
                if n == 0 {
                    return;
                }
                if let Some(c) = flow.child_at_index((cur + delta).clamp(0, n - 1)) {
                    flow.select_child(&c);
                    if let Some(b) = c.compute_bounds(&flow) {
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
                    refresh();
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
    let focus: gtk::Widget = flow.clone().upcast();
    (root, focus, refresh)
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

/// Folder button pinned (via layer-shell) to the top-right of the tracked editor window.
/// It expands into the file panel in place and goes away with the window.
pub fn overlay(cfg: OverlayCfg, rx: Receiver<Msg>) {
    let OverlayCfg { home, start, pal, on_pick, expand_on_start } = cfg;
    let rx = Rc::new(rx);
    launch(pal, move |app| {
        let (rx, on_pick, start) = (rx.clone(), on_pick.clone(), start.clone());
        if let Some(display) = gdk::Display::default() {
            let prov = gtk::CssProvider::new();
            prov.load_from_string("window.tui-overlay { background: transparent; box-shadow: none; } .tui-panel { background: @window_bg_color; color: @window_fg_color; border-radius: 14px; box-shadow: 0 2px 14px rgba(0,0,0,.35); } .tui-panel label, .tui-panel entry, .tui-panel text, .tui-panel button { color: @window_fg_color; }");
            gtk::style_context_add_provider_for_display(&display, &prov, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1);
        }
        let win = gtk::ApplicationWindow::builder().application(app).decorated(false).resizable(false).css_classes(["tui-overlay"]).build();
        win.init_layer_shell();
        win.set_layer(Layer::Overlay);
        win.set_anchor(Edge::Top, true);
        win.set_anchor(Edge::Right, true);
        win.set_keyboard_mode(KeyboardMode::OnDemand);
        win.set_namespace(Some("tui-shim-overlay"));

        let touched = Rc::new(Cell::new(Instant::now() - Duration::from_secs(60)));
        let panel_slot: Rc<RefCell<Option<gtk::Box>>> = Rc::new(RefCell::new(None));
        let collapse: Rc<dyn Fn()> = {
            let slot = panel_slot.clone();
            Rc::new(move || {
                if let Some(p) = slot.borrow().as_ref() {
                    p.set_visible(false);
                }
            })
        };
        let (c, t, h) = (collapse.clone(), touched.clone(), home.clone());
        let picked: PickFn = Rc::new(move |p| {
            c();
            t.set(Instant::now());
            files::push_recent(&h, &p);
            on_pick(p);
        });
        let (panel, focus, refresh) = file_panel(home.clone(), start, picked, collapse.clone());
        panel.add_css_class("tui-panel");
        panel.set_width_request(540);
        panel.set_visible(false);
        *panel_slot.borrow_mut() = Some(panel.clone());

        let toggle = gtk::Button::builder().icon_name("folder-open-symbolic").css_classes(["circular", "osd"]).halign(gtk::Align::End).tooltip_text("Open a file").build();
        {
            let (panel, focus, refresh, t) = (panel.clone(), focus.clone(), refresh.clone(), touched.clone());
            toggle.connect_clicked(move |_| {
                t.set(Instant::now());
                let show = !panel.is_visible();
                panel.set_visible(show);
                if show {
                    refresh();
                    focus_first(&focus);
                }
            });
        }
        let root = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
        root.append(&toggle);
        root.append(&panel);
        win.set_child(Some(&root));

        // debugging aid: open the panel as soon as the overlay first shows
        let auto_expand = Cell::new(expand_on_start || std::env::var_os("TUI_SHIM_OVERLAY_EXPANDED").is_some());
        let toggle2 = toggle.clone();
        let (w, app2, t) = (win.clone(), app.clone(), touched.clone());
        glib::timeout_add_local(Duration::from_millis(80), move || {
            while let Ok(m) = rx.try_recv() {
                match m {
                    Msg::Show { right, top, monitor } => {
                        w.set_margin(Edge::Right, right);
                        w.set_margin(Edge::Top, top);
                        if let Some(mon) = monitor.as_deref().and_then(monitor_by_name) {
                            w.set_monitor(Some(&mon));
                        }
                        if !w.is_visible() {
                            w.present();
                        }
                        if auto_expand.replace(false) {
                            toggle2.emit_clicked();
                        }
                    }
                    Msg::Hide => {
                        // clicking the overlay steals focus from the editor; don't vanish mid-use
                        let in_use = w.is_active() || t.get().elapsed() < Duration::from_millis(800);
                        if !in_use {
                            collapse();
                            w.set_visible(false);
                        }
                    }
                    Msg::Quit => {
                        app2.quit();
                        return glib::ControlFlow::Break;
                    }
                }
            }
            glib::ControlFlow::Continue
        });
        // hold the application open even while the window is hidden
        std::mem::forget(app.hold());
    });
}
