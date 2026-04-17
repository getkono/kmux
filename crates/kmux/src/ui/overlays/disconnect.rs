use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use super::centered_overlay;
use crate::theme::Theme;

pub fn render_disconnect_overlay(f: &mut Frame, area: Rect, theme: &Theme, reason: &str) {
    let width = 56u16.min(area.width.saturating_sub(4));
    let height = 9u16.min(area.height.saturating_sub(4));
    let overlay_area = centered_overlay(area, width, height);

    f.render_widget(Clear, overlay_area);

    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            " Connection lost ",
            Style::default().fg(theme.red).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("Reason: ", Style::default().fg(theme.fg_dim)),
            Span::styled(reason, Style::default().fg(theme.fg)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("Reconnect now? ", Style::default().fg(theme.fg)),
            Span::styled(
                "[y/Enter]",
                Style::default()
                    .fg(theme.green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ", Style::default()),
            Span::styled("[q] quit", Style::default().fg(theme.fg_dim)),
        ]),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.red))
        .style(Style::default().bg(theme.bg));

    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        overlay_area,
    );
}
