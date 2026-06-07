//! The GTK preferences window (theme + font), opened with Ctrl+,.
//!
//! Changes apply live and persist to `config.toml`: the theme is resolved into
//! `core.palette` (the pump reloads the chrome CSS + window styling and the
//! cairo grid reads it directly), and the font re-derives the cell metrics.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk4::{DrawingArea, StringList};

use kmux_app::{config, theme};

use super::{Frontend, render};

/// Build and present the preferences window.
pub fn open(fe: &Rc<RefCell<Frontend>>, drawing: &DrawingArea) {
    let window = adw::PreferencesWindow::new();
    window.set_title(Some("kmux Preferences"));
    window.set_search_enabled(false);
    window.set_default_size(420, 240);

    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::new();
    group.set_title("Appearance");

    group.add(&theme_row(fe, drawing));
    group.add(&font_row(fe, drawing));

    page.add(&group);
    window.add(&page);
    window.present();
}

/// Theme combo over the built-in themes; applies via `config::resolve_theme`.
fn theme_row(fe: &Rc<RefCell<Frontend>>, drawing: &DrawingArea) -> adw::ComboRow {
    let row = adw::ComboRow::new();
    row.set_title("Theme");
    let model = StringList::new(theme::BUILTIN_THEMES);
    row.set_model(Some(&model));

    // Pre-select the configured theme if it's a built-in.
    if let Some(name) = config::load().theme
        && let Some(i) = theme::BUILTIN_THEMES.iter().position(|t| *t == name)
    {
        row.set_selected(i as u32);
    }

    let fe = fe.clone();
    let drawing = drawing.clone();
    row.connect_selected_notify(move |row| {
        let Some(name) = theme::BUILTIN_THEMES.get(row.selected() as usize) else {
            return;
        };
        {
            let mut f = fe.borrow_mut();
            f.core.palette = config::resolve_theme(Some(name));
            f.core.needs_render = true;
        }
        let mut cfg = config::load();
        cfg.theme = Some((*name).to_string());
        let _ = config::save(&cfg);
        drawing.queue_draw();
    });
    row
}

/// Font entry; on apply re-derives the cell metrics and persists.
fn font_row(fe: &Rc<RefCell<Frontend>>, drawing: &DrawingArea) -> adw::EntryRow {
    let row = adw::EntryRow::new();
    row.set_title("Font");
    row.set_show_apply_button(true);
    let current = fe.borrow().metrics.font.to_str();
    row.set_text(current.as_str());

    let fe = fe.clone();
    let drawing = drawing.clone();
    row.connect_apply(move |row| {
        let spec = row.text().to_string();
        {
            let mut f = fe.borrow_mut();
            let font = render::font_from_str(&spec);
            f.metrics = render::Metrics::measure(&drawing.pango_context(), font);
            f.core.needs_render = true;
        }
        let mut cfg = config::load();
        cfg.font = Some(spec);
        let _ = config::save(&cfg);
        // Re-evaluate cols/rows at the new cell size, then repaint.
        drawing.queue_resize();
        drawing.queue_draw();
    });
    row
}
