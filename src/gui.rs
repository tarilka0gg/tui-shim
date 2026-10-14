//! Native libadwaita pickers (4:3 landscape window, 640x480). Compiled with the `gui` feature.
use crate::palette::Palette;
use crate::picker::Item;
use anyhow::Result;
use libadwaita as adw;
use adw::gtk::{self, gdk, gio, glib};
use adw::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

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
