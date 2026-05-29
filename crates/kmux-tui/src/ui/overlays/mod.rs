mod command;
mod disconnect;
mod help;
mod metrics;
mod pickers;
pub(super) use command::render_command_overlay;
pub(super) use disconnect::{render_connecting_overlay, render_disconnect_overlay};
pub(super) use help::{
    render_confirm_overlay, render_help_overlay, render_hud, render_rename_overlay,
};
pub(super) use metrics::render_metrics_overlay;
pub(super) use pickers::{
    render_dir_picker_overlay, render_server_picker_overlay, render_session_picker_overlay,
};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::theme::Theme;

/// Centre a `width × height` popup within `area`.
pub(super) fn centered_overlay(area: Rect, width: u16, height: u16) -> Rect {
    let x = area.width.saturating_sub(width) / 2;
    let y = area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width, height)
}

/// Pre-formatted item for `render_list_picker`.
pub(super) struct PickerItem {
    pub text: String,
    pub style: Style,
}

/// Render a generic search-input + list picker overlay.
///
/// Clears the overlay area, draws an input line (`input_label` + `input_text` + cursor),
/// a separator, the item list (or `empty_msg` when empty), and a titled border.
///
/// Returns the absolute screen row of the first rendered item, so callers can
/// register per-item click/hover hit-boxes. When `items` is empty, returns
/// `None` (the overlay shows `empty_msg` instead of item rows).
#[allow(clippy::too_many_arguments)]
pub(super) fn render_list_picker(
    f: &mut Frame,
    overlay_area: Rect,
    theme: &Theme,
    title: &str,
    border_color: Color,
    input_label: &str,
    input_text: &str,
    items: &[PickerItem],
    empty_msg: &str,
) -> Option<u16> {
    f.render_widget(Clear, overlay_area);
    let inner_width = overlay_area.width.saturating_sub(2) as usize;

    use ratatui::text::Line;
    let mut lines = vec![];
    lines.push(Line::from(vec![
        Span::styled(input_label, Style::default().fg(theme.fg_dim).bg(theme.bg)),
        Span::styled(
            format!("{input_text}_"),
            Style::default().fg(theme.fg).bg(theme.bg),
        ),
    ]));
    lines.push(Line::from(Span::styled(
        "\u{2500}".repeat(inner_width),
        Style::default().fg(theme.fg_dim).bg(theme.bg),
    )));
    if items.is_empty() {
        lines.push(Line::from(Span::styled(
            empty_msg,
            Style::default().fg(theme.fg_dim).bg(theme.bg),
        )));
    } else {
        for item in items {
            let text: String = item
                .text
                .chars()
                .take(inner_width.saturating_sub(1))
                .collect();
            lines.push(Line::from(Span::styled(text, item.style)));
        }
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(
            title,
            Style::default()
                .fg(border_color)
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme.bg));

    f.render_widget(Paragraph::new(lines).block(block), overlay_area);

    // The items start on overlay row: border (1) + input line (1) + separator (1) = 3.
    if items.is_empty() {
        None
    } else {
        Some(overlay_area.y + 3)
    }
}
