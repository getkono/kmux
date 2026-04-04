use kmux_protocol::messages::{CellAttrs, CursorShape};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::app::App;
use crate::mode::{self, ConnectField, Mode};
use crate::theme;

pub fn render(f: &mut Frame, app: &App) {
    match &app.mode {
        Mode::Connect { field } => render_connect(f, app, field),
        _ => render_terminal(f, app),
    }
}

fn render_connect(f: &mut Frame, app: &App, active_field: &ConnectField) {
    let area = f.area();
    f.render_widget(Block::default().style(Style::default().bg(theme::BG)), area);

    let center_y = area.height / 2;
    let center_x = area.width / 2;
    let form_width = 40u16.min(area.width.saturating_sub(4));
    let form_x = center_x.saturating_sub(form_width / 2);
    let form_y = center_y.saturating_sub(8);

    let title = Line::from(vec![
        Span::styled(
            "kmux",
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" remote terminal", Style::default().fg(theme::FG_DIM)),
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

        let label_style = Style::default().fg(theme::FG);
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
            theme::ACCENT
        } else {
            theme::FG_DIM
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
    let status_y = form_y + 2 + 9;
    let status_style = if app.status_msg.starts_with("Connection failed")
        || app.status_msg.starts_with("Auth failed")
    {
        Style::default().fg(theme::RED)
    } else {
        Style::default().fg(theme::FG_DIM)
    };
    f.render_widget(
        Paragraph::new(Span::styled(&app.status_msg, status_style)),
        Rect::new(form_x, status_y, form_width, 2),
    );

    // Instructions
    let hint_y = status_y + 2;
    f.render_widget(
        Paragraph::new(Span::styled(
            "Tab: next field  Enter: connect",
            Style::default().fg(theme::FG_DIM),
        )),
        Rect::new(form_x, hint_y, form_width, 1),
    );
}

fn render_terminal(f: &mut Frame, app: &App) {
    let area = f.area();

    // Layout: session bar (1) | terminal (fill) | status bar (1) | hint bar (1)
    let chunks = Layout::vertical([
        Constraint::Length(1), // session bar
        Constraint::Min(1),    // terminal
        Constraint::Length(1), // status bar
        Constraint::Length(1), // hint bar
    ])
    .split(area);

    render_session_bar(f, app, chunks[0]);
    render_grid(f, app, chunks[1]);
    render_status_bar(f, app, chunks[2]);
    render_hint_bar(f, app, chunks[3]);

    // Overlays
    match &app.mode {
        Mode::Help => render_help_overlay(f, area),
        Mode::ConfirmClose { session } => render_confirm_overlay(f, area, session),
        Mode::Rename { buffer, session } => render_rename_overlay(f, area, session, buffer),
        _ => {}
    }

    // HUD overlay
    if app.hud_visible {
        render_hud(f, app, chunks[1]);
    }
}

fn render_session_bar(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = Vec::new();

    for info in app.session_list.iter() {
        let is_active = app.active_session.as_deref() == Some(&info.name);
        let style = if is_active {
            Style::default()
                .fg(theme::BG)
                .bg(theme::ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::FG).bg(theme::STATUS_BG)
        };
        spans.push(Span::styled(format!(" {} ", info.name), style));
        spans.push(Span::styled(" ", Style::default().bg(theme::STATUS_BG)));
    }

    if spans.is_empty() {
        spans.push(Span::styled(
            " No sessions ",
            Style::default().fg(theme::FG_DIM).bg(theme::STATUS_BG),
        ));
    }

    // Pad the rest
    let used: usize = spans.iter().map(|s| s.content.len()).sum();
    if used < area.width as usize {
        spans.push(Span::styled(
            " ".repeat(area.width as usize - used),
            Style::default().bg(theme::STATUS_BG),
        ));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_grid(f: &mut Frame, app: &App, area: Rect) {
    // Track cursor position to set after buffer operations
    let mut cursor_pos: Option<(u16, u16)> = None;

    {
        let buf = f.buffer_mut();

        // Fill background
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                let cell = &mut buf[(x, y)];
                cell.set_char(' ');
                cell.set_bg(theme::BG);
                cell.set_fg(theme::FG);
            }
        }

        let Some(name) = &app.active_session else {
            // No active session message
            let msg = "No active session -- press Ctrl+G then s, c to create one";
            let x = area.left() + area.width.saturating_sub(msg.len() as u16) / 2;
            let y = area.top() + area.height / 2;
            if y < area.bottom() {
                for (i, ch) in msg.chars().enumerate() {
                    let px = x + i as u16;
                    if px < area.right() {
                        let cell = &mut buf[(px, y)];
                        cell.set_char(ch);
                        cell.set_fg(theme::FG_DIM);
                    }
                }
            }
            return;
        };

        let Some(grid) = app.buffers.get(name) else {
            return;
        };

        let cells = grid.cells();
        let rows = grid.rows;
        let cols = grid.cols;
        let scroll_offset = grid.scroll_offset();
        let scrollback = grid.scrollback();

        // Render cells
        for vr in 0..rows.min(area.height as usize) {
            for vc in 0..cols.min(area.width as usize) {
                let screen_x = area.left() + vc as u16;
                let screen_y = area.top() + vr as u16;
                if screen_x >= area.right() || screen_y >= area.bottom() {
                    continue;
                }

                let cell_state = if scroll_offset > 0 && vr < scroll_offset {
                    let sb_len = scrollback.len();
                    let sb_idx = sb_len.saturating_sub(scroll_offset) + vr;
                    scrollback.get(sb_idx).and_then(|line| line.get(vc))
                } else {
                    let grid_row = if scroll_offset > 0 {
                        vr - scroll_offset
                    } else {
                        vr
                    };
                    cells.get(grid_row * cols + vc)
                };

                if let Some(cs) = cell_state {
                    let ratatui_cell = &mut buf[(screen_x, screen_y)];

                    if cs.attrs.contains(CellAttrs::WIDE_CHAR_SPACER) {
                        continue;
                    }

                    ratatui_cell.set_char(cs.c);
                    ratatui_cell.set_fg(theme::cell_color(cs.fg));
                    ratatui_cell.set_bg(theme::cell_color(cs.bg));

                    let mut modifier = Modifier::empty();
                    if cs.attrs.contains(CellAttrs::BOLD) {
                        modifier |= Modifier::BOLD;
                    }
                    if cs.attrs.contains(CellAttrs::ITALIC) {
                        modifier |= Modifier::ITALIC;
                    }
                    if cs.attrs.contains(CellAttrs::UNDERLINE) {
                        modifier |= Modifier::UNDERLINED;
                    }
                    if cs.attrs.contains(CellAttrs::STRIKETHROUGH) {
                        modifier |= Modifier::CROSSED_OUT;
                    }
                    if cs.attrs.contains(CellAttrs::DIM) {
                        modifier |= Modifier::DIM;
                    }
                    if cs.attrs.contains(CellAttrs::HIDDEN) {
                        modifier |= Modifier::HIDDEN;
                    }
                    ratatui_cell.set_style(Style::default().add_modifier(modifier));
                }
            }
        }

        // Render cursor
        let cursor = grid.cursor();
        if scroll_offset == 0 && cursor.visible && cursor.shape != CursorShape::Hidden {
            let cur_row = cursor.row as usize;
            let cur_col = cursor.col as usize;
            if cur_row < area.height as usize && cur_col < area.width as usize {
                let cx = area.left() + cur_col as u16;
                let cy = area.top() + cur_row as u16;
                if cx < area.right() && cy < area.bottom() {
                    match cursor.shape {
                        CursorShape::Block => {
                            let cell = &mut buf[(cx, cy)];
                            let fg = cell.bg;
                            let bg = Color::White;
                            cell.set_fg(fg);
                            cell.set_bg(bg);
                        }
                        CursorShape::Underline | CursorShape::Bar => {
                            cursor_pos = Some((cx, cy));
                        }
                        CursorShape::HollowBlock => {
                            let cell = &mut buf[(cx, cy)];
                            cell.set_style(
                                Style::default()
                                    .fg(Color::White)
                                    .add_modifier(Modifier::SLOW_BLINK),
                            );
                        }
                        CursorShape::Hidden => {}
                    }
                }
            }
        }

        // Scroll indicator
        if scroll_offset > 0 {
            let label = format!("[{}/{}]", scroll_offset, scrollback.len());
            let x = area.right().saturating_sub(label.len() as u16 + 1);
            let y = area.top();
            if y < area.bottom() {
                for (i, ch) in label.chars().enumerate() {
                    let px = x + i as u16;
                    if px < area.right() {
                        let cell = &mut buf[(px, y)];
                        cell.set_char(ch);
                        cell.set_fg(theme::YELLOW);
                        cell.set_bg(Color::Rgb(0, 0, 0));
                    }
                }
            }
        }
    } // buf borrow ends here

    if let Some((cx, cy)) = cursor_pos {
        f.set_cursor_position((cx, cy));
    }
}

fn render_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = Vec::new();

    // Connection info
    let host_port = app.host_port_display();
    if !host_port.is_empty() {
        spans.push(Span::styled(
            format!(" {} ", host_port),
            Style::default().fg(theme::GREEN).bg(theme::STATUS_BG),
        ));
        spans.push(Span::styled(
            " | ",
            Style::default().fg(theme::FG_DIM).bg(theme::STATUS_BG),
        ));
    }

    // Session count
    spans.push(Span::styled(
        format!("{} sessions", app.session_list.len()),
        Style::default().fg(theme::FG).bg(theme::STATUS_BG),
    ));

    // Input lock
    if let Some(session) = &app.active_session
        && app.input_locked.get(session).copied().unwrap_or(false)
    {
        spans.push(Span::styled(
            " | ",
            Style::default().fg(theme::FG_DIM).bg(theme::STATUS_BG),
        ));
        spans.push(Span::styled(
            "LOCKED",
            Style::default()
                .fg(theme::RED)
                .bg(theme::STATUS_BG)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // Size
    if let Some((rows, cols)) = app.active_term_size() {
        spans.push(Span::styled(
            " | ",
            Style::default().fg(theme::FG_DIM).bg(theme::STATUS_BG),
        ));
        spans.push(Span::styled(
            format!("{cols}x{rows}"),
            Style::default().fg(theme::FG_DIM).bg(theme::STATUS_BG),
        ));
    }

    // Status message (right-aligned)
    if !app.status_msg.is_empty() {
        let used: usize = spans.iter().map(|s| s.content.len()).sum();
        let msg_len = app.status_msg.len() + 2;
        let gap = (area.width as usize).saturating_sub(used + msg_len);
        if gap > 0 {
            spans.push(Span::styled(
                " ".repeat(gap),
                Style::default().bg(theme::STATUS_BG),
            ));
        }
        spans.push(Span::styled(
            format!(" {} ", app.status_msg),
            Style::default().fg(theme::FG_DIM).bg(theme::STATUS_BG),
        ));
    }

    // Pad
    let used: usize = spans.iter().map(|s| s.content.len()).sum();
    if used < area.width as usize {
        spans.push(Span::styled(
            " ".repeat(area.width as usize - used),
            Style::default().bg(theme::STATUS_BG),
        ));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_hint_bar(f: &mut Frame, app: &App, area: Rect) {
    let hints = mode::mode_hints(&app.mode);
    let mode_name = mode::mode_name(&app.mode);

    let mut spans = vec![Span::styled(
        format!(" {} ", mode_name),
        Style::default()
            .fg(theme::BG)
            .bg(mode_color(&app.mode))
            .add_modifier(Modifier::BOLD),
    )];

    for (key, desc) in &hints {
        spans.push(Span::styled(" ", Style::default().bg(theme::BG)));
        spans.push(Span::styled(
            format!(" {key} "),
            Style::default()
                .fg(theme::BG)
                .bg(theme::FG_DIM)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {desc}"),
            Style::default().fg(theme::FG).bg(theme::BG),
        ));
    }

    // Pad
    let used: usize = spans.iter().map(|s| s.content.len()).sum();
    if used < area.width as usize {
        spans.push(Span::styled(
            " ".repeat(area.width as usize - used),
            Style::default().bg(theme::BG),
        ));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn mode_color(mode: &Mode) -> Color {
    match mode {
        Mode::Normal => theme::GREEN,
        Mode::Locked => theme::RED,
        Mode::Select => theme::ACCENT,
        Mode::Session => theme::PURPLE,
        Mode::Scroll => theme::YELLOW,
        Mode::Signal => theme::RED,
        Mode::ConfirmClose { .. } => theme::RED,
        Mode::Rename { .. } => theme::ORANGE,
        Mode::Help => theme::ACCENT,
        Mode::Connect { .. } => theme::ACCENT,
    }
}

fn render_help_overlay(f: &mut Frame, area: Rect) {
    let entries = mode::help_entries();

    let width = 50u16.min(area.width.saturating_sub(4));
    let height = (entries.len() as u16 + 4).min(area.height.saturating_sub(4));
    let x = area.width.saturating_sub(width) / 2;
    let y = area.height.saturating_sub(height) / 2;
    let overlay_area = Rect::new(x, y, width, height);

    f.render_widget(Clear, overlay_area);

    let mut lines = vec![Line::from(vec![Span::styled(
        " Keyboard Shortcuts ",
        Style::default()
            .fg(theme::ACCENT)
            .add_modifier(Modifier::BOLD),
    )])];

    lines.push(Line::from(""));

    for (key, desc) in &entries {
        if key.is_empty() {
            lines.push(Line::from(""));
        } else if desc.is_empty() {
            lines.push(Line::from(Span::styled(
                *key,
                Style::default()
                    .fg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD),
            )));
        } else {
            lines.push(Line::from(vec![
                Span::styled(format!("{:>14}", key), Style::default().fg(theme::GREEN)),
                Span::styled(format!("  {}", desc), Style::default().fg(theme::FG)),
            ]));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Press any key to close ",
        Style::default().fg(theme::FG_DIM),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::ACCENT))
        .style(Style::default().bg(theme::BG));

    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        overlay_area,
    );
}

fn render_confirm_overlay(f: &mut Frame, area: Rect, session: &str) {
    let width = 40u16.min(area.width.saturating_sub(4));
    let height = 5;
    let x = area.width.saturating_sub(width) / 2;
    let y = area.height.saturating_sub(height) / 2;
    let overlay_area = Rect::new(x, y, width, height);

    f.render_widget(Clear, overlay_area);

    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(
                " Close session ",
                Style::default().fg(theme::RED).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("'{session}'"), Style::default().fg(theme::FG)),
            Span::styled("? (y/n)", Style::default().fg(theme::FG_DIM)),
        ]),
        Line::from(""),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::RED))
        .style(Style::default().bg(theme::BG));

    f.render_widget(Paragraph::new(lines).block(block), overlay_area);
}

fn render_rename_overlay(f: &mut Frame, area: Rect, session: &str, buffer: &str) {
    let width = 40u16.min(area.width.saturating_sub(4));
    let height = 5;
    let x = area.width.saturating_sub(width) / 2;
    let y = area.height.saturating_sub(height) / 2;
    let overlay_area = Rect::new(x, y, width, height);

    f.render_widget(Clear, overlay_area);

    let lines = vec![
        Line::from(Span::styled(
            format!(" Rename '{session}' "),
            Style::default()
                .fg(theme::ORANGE)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(buffer, Style::default().fg(theme::FG))),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::ORANGE))
        .style(Style::default().bg(theme::BG));

    f.render_widget(Paragraph::new(lines).block(block), overlay_area);

    // Cursor in rename field
    let cursor_x = x + 1 + buffer.len() as u16;
    let cursor_y = y + 3;
    if cursor_x < x + width - 1 {
        f.set_cursor_position((cursor_x, cursor_y));
    }
}

fn render_hud(f: &mut Frame, app: &App, area: Rect) {
    let snap = app.metrics.snapshot(app.force_snapshot_mode);
    let c = &snap.counters;

    let lines = vec![
        Line::from(Span::styled(
            format!(
                "Net+Apply: {:.1}ms avg / {:.1}ms max",
                snap.net_apply_avg_ms, snap.net_apply_max_ms
            ),
            Style::default().fg(theme::GREEN),
        )),
        Line::from(Span::styled(
            format!("Apply:     {:.2}ms avg", snap.apply_avg_ms),
            Style::default().fg(theme::GREEN),
        )),
        Line::from(Span::styled(
            format!("Batch:     {:.1} msgs avg", snap.batch_avg),
            Style::default().fg(theme::GREEN),
        )),
        Line::from(Span::styled(
            format!("Diff:      {} ops", snap.last_diff_ops),
            Style::default().fg(theme::GREEN),
        )),
        Line::from(Span::styled(
            format!("LargeDiff: {:.1}ms", snap.last_large_diff_ms),
            Style::default().fg(if snap.last_large_diff_ms > 16.0 {
                theme::YELLOW
            } else {
                theme::GREEN
            }),
        )),
        Line::from(Span::styled(
            format!(
                "Snapshot:  {}",
                if snap.snapshot_mode { "FORCED" } else { "off" }
            ),
            Style::default().fg(if snap.snapshot_mode {
                theme::YELLOW
            } else {
                theme::GREEN
            }),
        )),
        Line::from(Span::styled(
            format!(
                "Disc:{}  Gap:{}  Lag:{}  Sync:{}",
                c.stale_discards, c.seqno_gaps, c.lag_events, c.resyncs
            ),
            Style::default().fg(theme::YELLOW),
        )),
    ];

    let hud_width = 42u16.min(area.width);
    let hud_height = (lines.len() as u16 + 2).min(area.height);
    let hud_x = area.right().saturating_sub(hud_width + 1);
    let hud_y = area.top() + 1;
    let hud_area = Rect::new(hud_x, hud_y, hud_width, hud_height);

    f.render_widget(Clear, hud_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::FG_DIM))
        .title(" HUD ")
        .title_style(
            Style::default()
                .fg(theme::GREEN)
                .add_modifier(Modifier::BOLD),
        )
        .style(Style::default().bg(Color::Rgb(0x1a, 0x1d, 0x23)));

    f.render_widget(Paragraph::new(lines).block(block), hud_area);
}
