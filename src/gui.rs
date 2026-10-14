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

fn kind_icon(k: &Kind) -> &'static str {
    match k {
        Kind::Up => "go-up-symbolic",
        Kind::Dir => "folder-symbolic",
        Kind::File => "text-x-generic-symbolic",
        Kind::Recent => "document-open-recent-symbolic",
        Kind::New => "document-new-symbolic",
    }
}

/// Path line, filter/new-name entry, result list. Shared by the standalone window and the overlay.
/// Returns the widget, the entry (to focus) and a refresh hook (call when it becomes visible).
fn file_panel(home: PathBuf, start: PathBuf, on_pick: PickFn, on_cancel: Rc<dyn Fn()>) -> (gtk::Box, gtk::SearchEntry, Rc<dyn Fn()>) {
    let dir = Rc::new(RefCell::new(start));
    let entries: Rc<RefCell<Vec<Entry>>> = Rc::new(RefCell::new(Vec::new()));
    let path_label = gtk::Label::builder()
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::Start)
        .css_classes(["dim-label", "caption"])
        .build();
    let entry = gtk::SearchEntry::builder().placeholder_text("Filter, or type a new file name").hexpand(true).build();
    let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::Single).css_classes(["boxed-list"]).build();
    let scroller = gtk::ScrolledWindow::builder()
        .child(&list)
        .vexpand(true)
        .min_content_height(240)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    let root = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).margin_top(12).margin_bottom(12).margin_start(12).margin_end(12).build();
    root.append(&path_label);
    root.append(&entry);
    root.append(&scroller);

    let refresh: Rc<dyn Fn()> = {
        let (dir, entries, list, entry, path_label) = (dir.clone(), entries.clone(), list.clone(), entry.clone(), path_label.clone());
        Rc::new(move || {
            list.remove_all();
            let d = dir.borrow().clone();
            path_label.set_text(&d.display().to_string());
            let found = files::candidates(&home, &d, &entry.text());
            for e in &found {
                let row = adw::ActionRow::builder().title(&e.label).subtitle(&e.detail).use_markup(false).subtitle_lines(1).activatable(true).build();
                row.add_prefix(&gtk::Image::from_icon_name(kind_icon(&e.kind)));
                list.append(&row);
            }
            *entries.borrow_mut() = found;
            if let Some(r) = list.row_at_index(0) {
                list.select_row(Some(&r));
            }
        })
    };
    let activate: Rc<dyn Fn(usize)> = {
        let (dir, entries, entry, refresh) = (dir.clone(), entries.clone(), entry.clone(), refresh.clone());
        Rc::new(move |i| {
            let Some(e) = entries.borrow().get(i).cloned() else { return };
            match e.kind {
                Kind::Up | Kind::Dir => {
                    *dir.borrow_mut() = e.path;
                    entry.set_text("");
                    refresh();
                }
                _ => on_pick(e.path),
            }
        })
    };
    {
        let r = refresh.clone();
        entry.connect_search_changed(move |_| r());
    }
    {
        let (a, l) = (activate.clone(), list.clone());
        entry.connect_activate(move |_| {
            if let Some(row) = l.selected_row() {
                a(row.index() as usize);
            }
        });
    }
    {
        let a = activate.clone();
        list.connect_row_activated(move |_, row| a(row.index() as usize));
    }
    let kc = gtk::EventControllerKey::new();
    kc.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let (list, scroller) = (list.clone(), scroller.clone());
        kc.connect_key_pressed(move |_, key, _, _| {
            let step: i32 = match key {
                gdk::Key::Down => 1,
                gdk::Key::Up => -1,
                gdk::Key::Escape => {
                    on_cancel();
                    return glib::Propagation::Stop;
                }
                _ => return glib::Propagation::Proceed,
            };
            let cur = list.selected_row().map(|r| r.index()).unwrap_or(-1);
            if let Some(row) = list.row_at_index((cur + step).max(0)) {
                list.select_row(Some(&row));
                if let Some(b) = row.compute_bounds(&list) {
                    scroller.vadjustment().set_value((b.y() - 60.0) as f64);
                }
            }
            glib::Propagation::Stop
        });
    }
    root.add_controller(kc);
    (root, entry, refresh)
}

/// Standalone floating picker (used before an editor with no file browser starts).
pub fn pick_file(home: PathBuf, start: PathBuf, pal: Option<&Palette>) -> Result<Option<PathBuf>> {
    let result: Rc<RefCell<Option<PathBuf>>> = Rc::new(RefCell::new(None));
    let res = result.clone();
    launch(pal.cloned(), move |app| {
        let win: Rc<RefCell<Option<adw::ApplicationWindow>>> = Rc::new(RefCell::new(None));
        let (w1, r1) = (win.clone(), res.clone());
        let on_pick: PickFn = Rc::new(move |p| {
            *r1.borrow_mut() = Some(p);
            if let Some(w) = w1.borrow().as_ref() {
                w.close();
            }
        });
        let w2 = win.clone();
        let on_cancel: Rc<dyn Fn()> = Rc::new(move || {
            if let Some(w) = w2.borrow().as_ref() {
                w.close();
            }
        });
        let (panel, entry, refresh) = file_panel(home.clone(), start.clone(), on_pick, on_cancel);
        let window = window(app, "Open file", &panel);
        *win.borrow_mut() = Some(window.clone());
        refresh();
        window.present();
        float_self();
        entry.grab_focus();
    });
    let out = result.borrow().clone();
    Ok(out)
}

// -------------------------------------------------------------------- overlay

pub struct OverlayCfg {
    pub home: PathBuf,
    pub start: PathBuf,
    pub pal: Option<Palette>,
    pub on_pick: PickFn,
}

fn monitor_by_name(name: &str) -> Option<gdk::Monitor> {
    let mons = gdk::Display::default()?.monitors();
    (0..mons.n_items()).filter_map(|i| mons.item(i)?.downcast::<gdk::Monitor>().ok()).find(|m| m.connector().as_deref() == Some(name))
}

/// Folder button pinned (via layer-shell) to the top-right of the tracked editor window.
/// It expands into the file panel in place and goes away with the window.
pub fn overlay(cfg: OverlayCfg, rx: Receiver<Msg>) {
    let OverlayCfg { home, start, pal, on_pick } = cfg;
    let rx = Rc::new(rx);
    launch(pal, move |app| {
        let (rx, on_pick, start) = (rx.clone(), on_pick.clone(), start.clone());
        if let Some(display) = gdk::Display::default() {
            let prov = gtk::CssProvider::new();
            prov.load_from_string("window.tui-overlay { background: transparent; box-shadow: none; } .tui-panel { background: @window_bg_color; border-radius: 14px; box-shadow: 0 2px 14px rgba(0,0,0,.35); }");
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
        let (panel, entry, refresh) = file_panel(home.clone(), start, picked, collapse.clone());
        panel.add_css_class("tui-panel");
        panel.set_width_request(460);
        panel.set_visible(false);
        *panel_slot.borrow_mut() = Some(panel.clone());

        let toggle = gtk::Button::builder().icon_name("folder-open-symbolic").css_classes(["circular", "osd"]).halign(gtk::Align::End).tooltip_text("Open a file").build();
        {
            let (panel, entry, refresh, t) = (panel.clone(), entry.clone(), refresh.clone(), touched.clone());
            toggle.connect_clicked(move |_| {
                t.set(Instant::now());
                let show = !panel.is_visible();
                panel.set_visible(show);
                if show {
                    entry.set_text("");
                    refresh();
                    entry.grab_focus();
                }
            });
        }
        let root = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
        root.append(&toggle);
        root.append(&panel);
        win.set_child(Some(&root));

        // debugging aid: open the panel as soon as the overlay first shows
        let auto_expand = Cell::new(std::env::var_os("TUI_SHIM_OVERLAY_EXPANDED").is_some());
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
