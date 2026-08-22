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

use super::shell::Shell;
use super::{Frontend, render};

/// Persist `cfg`, reporting a write failure instead of discarding it.
///
/// Every preference here applies live *and* is meant to survive a restart. When
/// the write failed silently the two halves disagreed: the UI showed the new
/// value, the file kept the old one, and the setting reverted on next launch
/// with nothing logged and nothing shown. Surface it in both channels — a toast
/// for the person who just clicked, and the log for `kmux client logs`.
fn persist(shell: &Rc<Shell>, cfg: &config::KmuxConfig) {
    if let Err(e) = config::save(cfg) {
        tracing::error!(error = %e, "failed to persist preferences");
        shell
            .toasts
            .add_toast(adw::Toast::new(&format!("Could not save preferences: {e}")));
    }
}

/// Build and present the preferences window.
pub fn open(fe: &Rc<RefCell<Frontend>>, shell: &Rc<Shell>) {
    let drawing = &shell.drawing;
    let window = adw::PreferencesWindow::new();
    window.set_title(Some("kmux Preferences"));
    window.set_search_enabled(false);
    window.set_default_size(420, 240);

    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::new();
    group.set_title("Appearance");

    group.add(&theme_row(fe, shell, drawing));
    group.add(&font_row(fe, shell, drawing));
    group.add(&cursor_blink_row(fe, shell, drawing));
    page.add(&group);

    let perf = adw::PreferencesGroup::new();
    perf.set_title("Performance");
    perf.add(&perf_counters_row(fe, shell, drawing));
    page.add(&perf);

    window.add(&page);
    window.present();
}

/// Theme combo over the built-in themes; applies via `config::resolve_theme`.
fn theme_row(
    fe: &Rc<RefCell<Frontend>>,
    shell: &Rc<Shell>,
    drawing: &DrawingArea,
) -> adw::ComboRow {
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
    let shell = shell.clone();
    let drawing = drawing.clone();
    row.connect_selected_notify(move |row| {
        let Some(name) = theme::BUILTIN_THEMES.get(row.selected() as usize) else {
            return;
        };
        {
            let mut f = fe.borrow_mut();
            f.core.palette = config::resolve_theme(Some(name));
            f.core.request_render();
        }
        let mut cfg = config::load();
        cfg.theme = Some((*name).to_string());
        persist(&shell, &cfg);
        drawing.queue_draw();
    });
    row
}

/// Font entry; on apply re-derives the cell metrics and persists.
fn font_row(fe: &Rc<RefCell<Frontend>>, shell: &Rc<Shell>, drawing: &DrawingArea) -> adw::EntryRow {
    let row = adw::EntryRow::new();
    row.set_title("Font");
    row.set_show_apply_button(true);
    let current = fe.borrow().metrics.font.to_str();
    row.set_text(current.as_str());

    let fe = fe.clone();
    let shell = shell.clone();
    let drawing = drawing.clone();
    row.connect_apply(move |row| {
        let spec = row.text().to_string();
        // Persist the legacy font string first, then re-resolve the full
        // appearance so any structured `font-*` / `adjust-cell-*` keys in
        // config.toml still apply on top of the edited family + size.
        let mut cfg = config::load();
        cfg.font = Some(spec);
        persist(&shell, &cfg);
        let appearance = config::resolve_appearance(None);
        {
            let mut f = fe.borrow_mut();
            f.metrics = render::Metrics::measure(&drawing.pango_context(), &appearance);
            f.core.appearance = appearance;
            f.core.request_render();
        }
        // Re-evaluate cols/rows at the new cell size, then repaint.
        drawing.queue_resize();
        drawing.queue_draw();
    });
    row
}

/// HUD performance-counters switch (issue #61); toggles `core.show_perf_counters`
/// live (hiding the latency + FPS counters also stops their computation) and
/// persists.
fn perf_counters_row(
    fe: &Rc<RefCell<Frontend>>,
    shell: &Rc<Shell>,
    drawing: &DrawingArea,
) -> adw::SwitchRow {
    let row = adw::SwitchRow::new();
    row.set_title("HUD latency & FPS counters");
    row.set_subtitle("Hiding them also skips their per-frame calculation");
    row.set_active(fe.borrow().core.show_perf_counters);

    let fe = fe.clone();
    let shell = shell.clone();
    let drawing = drawing.clone();
    row.connect_active_notify(move |row| {
        let on = row.is_active();
        {
            let mut f = fe.borrow_mut();
            f.core.show_perf_counters = on;
            f.core.request_render();
        }
        let mut cfg = config::load();
        cfg.perf_counters = Some(on);
        persist(&shell, &cfg);
        drawing.queue_draw();
    });
    row
}

/// Cursor-blink switch; toggles `core.cursor_blink_enabled` live and persists.
fn cursor_blink_row(
    fe: &Rc<RefCell<Frontend>>,
    shell: &Rc<Shell>,
    drawing: &DrawingArea,
) -> adw::SwitchRow {
    let row = adw::SwitchRow::new();
    row.set_title("Blink cursor");
    row.set_active(fe.borrow().core.cursor_blink_enabled);

    let fe = fe.clone();
    let shell = shell.clone();
    let drawing = drawing.clone();
    row.connect_active_notify(move |row| {
        let on = row.is_active();
        {
            let mut f = fe.borrow_mut();
            f.core.cursor_blink_enabled = on;
            f.core.request_render();
        }
        let mut cfg = config::load();
        cfg.cursor_blink = Some(on);
        persist(&shell, &cfg);
        drawing.queue_draw();
    });
    row
}
