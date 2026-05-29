//! Floating command-palette overlay. Reuses [`super::render_list_picker`] so
//! the visual style matches `SessionPicker` and `ServerPicker`.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use crate::app::App;
use crate::cmd::hint::{Hint, MAX_HINTS, build_hints};
use crate::mode::Mode;

use super::{PickerItem, centered_overlay, render_list_picker};

pub fn render_command_overlay(f: &mut Frame, area: Rect, app: &mut App) {
    // Bail early if we're not in command mode (defensive — caller should check).
    if !matches!(app.mode, Mode::Command(_)) {
        return;
    }

    let hints: Vec<Hint> = build_hints(app);
    let theme = &app.theme;
    let buffer_text: String = match &app.mode {
        Mode::Command(s) => s.buffer.clone(),
        _ => String::new(),
    };
    let selected = match &app.mode {
        Mode::Command(s) => s.selected.min(hints.len().saturating_sub(1)),
        _ => 0,
    };

    let items: Vec<PickerItem> = hints
        .iter()
        .take(MAX_HINTS)
        .enumerate()
        .map(|(i, h)| {
            let is_selected = i == selected;
            let cursor = if is_selected { ">" } else { " " };
            let text = if h.summary.is_empty() {
                format!("{cursor} {:<26}", h.display)
            } else {
                format!("{cursor} {:<26} {}", h.display, h.summary)
            };
            let style = if is_selected {
                Style::default()
                    .fg(theme.bg)
                    .bg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.fg).bg(theme.bg)
            };
            PickerItem { text, style }
        })
        .collect();

    let item_count = items.len();
    let width = 72u16.min(area.width.saturating_sub(4));
    // Border (2) + input (1) + separator (1) + items (clamped). Reserve room
    // for at least one row even when no hints so the empty-state line shows.
    let body_rows = item_count.clamp(1, MAX_HINTS) as u16;
    let height = (body_rows + 4).min(area.height.saturating_sub(2));
    let overlay_area = centered_overlay(area, width, height);

    render_list_picker(
        f,
        overlay_area,
        theme,
        " Command ",
        theme.accent,
        " /",
        &buffer_text,
        &items,
        " (no matches — Esc to cancel) ",
    );
}
