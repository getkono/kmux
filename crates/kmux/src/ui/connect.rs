use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::App;
use crate::mode::ConnectField;

pub(super) fn render_connect(f: &mut Frame, app: &App, active_field: &ConnectField) {
    let theme = &app.theme;
    let area = f.area();
    f.render_widget(Block::default().style(Style::default().bg(theme.bg)), area);

    let center_y = area.height / 2;
    let center_x = area.width / 2;
    let form_width = 40u16.min(area.width.saturating_sub(4));
    let form_x = center_x.saturating_sub(form_width / 2);
    let form_y = center_y.saturating_sub(8);

    let title = Line::from(vec![
        Span::styled(
            "kmux",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" remote terminal", Style::default().fg(theme.fg_dim)),
    ]);
    f.render_widget(
        Paragraph::new(title),
        Rect::new(form_x, form_y, form_width, 1),
    );

    let fields = [
        ("Host", &app.connect_host, ConnectField::Host),
        ("Port", &app.connect_port, ConnectField::Port),
        ("Token", &app.connect_token, ConnectField::Token),
    ];

    for (i, (label, value, field)) in fields.iter().enumerate() {
        let y = form_y + 2 + (i as u16 * 3);
        let is_active = active_field == field;

        let label_style = Style::default().fg(theme.fg);
        f.render_widget(
            Paragraph::new(Span::styled(*label, label_style)),
            Rect::new(form_x, y, form_width, 1),
        );

        let display = if matches!(field, ConnectField::Token) && !value.is_empty() {
            "*".repeat(value.len())
        } else {
            value.to_string()
        };

        let border_color = if is_active {
            theme.accent
        } else {
            theme.fg_dim
        };
        let input_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color));
        let input_area = Rect::new(form_x, y + 1, form_width, 3);
        f.render_widget(
            Paragraph::new(display.as_str()).block(input_block),
            input_area,
        );

        if is_active {
            // Show cursor
            let cursor_x = form_x + 1 + display.len() as u16;
            let cursor_y = y + 2;
            if cursor_x < form_x + form_width - 1 {
                f.set_cursor_position((cursor_x, cursor_y));
            }
        }
    }

    // Status message
    let status_msg = app.mgr.status_msg();
    let status_y = form_y + 2 + 9;
    let status_style =
        if status_msg.starts_with("Connection failed") || status_msg.starts_with("Auth failed") {
            Style::default().fg(theme.red)
        } else {
            Style::default().fg(theme.fg_dim)
        };
    f.render_widget(
        Paragraph::new(Span::styled(status_msg, status_style)),
        Rect::new(form_x, status_y, form_width, 2),
    );

    // Instructions
    let hint_y = status_y + 2;
    f.render_widget(
        Paragraph::new(Span::styled(
            "Tab: next field  Enter: connect",
            Style::default().fg(theme.fg_dim),
        )),
        Rect::new(form_x, hint_y, form_width, 1),
    );
}
