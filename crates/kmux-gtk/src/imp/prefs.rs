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
    persist_with(cfg, config::save, |message| {
        shell.toasts.add_toast(adw::Toast::new(message));
    });
}

/// [`persist`] with the save and the toast as parameters, so what a failed
/// write shows the user is testable without a display. Returns whether the
/// preferences reached disk.
fn persist_with(
    cfg: &config::KmuxConfig,
    save: impl FnOnce(&config::KmuxConfig) -> anyhow::Result<()>,
    toast: impl FnOnce(&str),
) -> bool {
    match save(cfg) {
        Ok(()) => true,
        Err(e) => {
            tracing::error!(error = %e, "failed to persist preferences");
            toast(&format!("Could not save preferences: {e}"));
            false
        }
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
            f.core
                .mutate(|c| c.palette = config::resolve_theme(Some(name)));
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
        // Persist the legacy font string, then resolve the full appearance from
        // the edited config value — not from disk — so the new font applies
        // live even when the save failed, and any structured `font-*` /
        // `adjust-cell-*` keys still apply on top of the edited family + size.
        let mut cfg = config::load();
        cfg.font = Some(spec);
        persist(&shell, &cfg);
        let appearance = config::resolve_appearance_from(&cfg, None);
        {
            let mut f = fe.borrow_mut();
            f.metrics = render::Metrics::measure(&drawing.pango_context(), &appearance);
            f.core.mutate(|c| c.appearance = appearance);
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
            f.core.mutate(|c| c.show_perf_counters = on);
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
            f.core.mutate(|c| c.cursor_blink_enabled = on);
            f.core.request_render();
        }
        let mut cfg = config::load();
        cfg.cursor_blink = Some(on);
        persist(&shell, &cfg);
        drawing.queue_draw();
    });
    row
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use kmux_app::config::KmuxConfig;

    use super::persist_with;

    /// A failed write used to be discarded: the setting looked applied and
    /// reverted on restart. The person who changed it is told why.
    #[test]
    fn a_failed_save_is_shown_to_the_user() {
        let shown = RefCell::new(Vec::new());
        let saved = persist_with(
            &KmuxConfig::default(),
            |_| Err(anyhow::anyhow!("read-only file system")),
            |message| shown.borrow_mut().push(message.to_string()),
        );
        assert!(!saved);
        assert_eq!(
            shown.into_inner(),
            ["Could not save preferences: read-only file system"]
        );
    }

    #[test]
    fn a_successful_save_shows_nothing() {
        let saved = persist_with(
            &KmuxConfig::default(),
            |_| Ok(()),
            |message| panic!("nothing to report, got {message:?}"),
        );
        assert!(saved);
    }

    /// The save is handed the config being persisted, not a reloaded one.
    #[test]
    fn the_edited_config_is_what_gets_saved() {
        let cfg = KmuxConfig {
            font: Some("Iosevka 14".to_string()),
            ..KmuxConfig::default()
        };
        let seen = RefCell::new(None);
        persist_with(
            &cfg,
            |c| {
                *seen.borrow_mut() = c.font.clone();
                Ok(())
            },
            |_| {},
        );
        assert_eq!(seen.into_inner().as_deref(), Some("Iosevka 14"));
    }
}
