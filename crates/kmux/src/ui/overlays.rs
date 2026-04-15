use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::app::App;
use crate::mode;
use crate::theme::Theme;

pub(super) fn render_session_picker_overlay(f: &mut Frame, area: Rect, app: &App) {
    let theme = &app.theme;
    let search = &app.session_picker_search;
    let search_lower = search.to_lowercase();

    let matches: Vec<_> = app
        .mgr
        .session_list()
        .iter()
        .filter(|e| {
            search_lower.is_empty()
                || e.meta.name.to_lowercase().contains(&search_lower)
                || e.meta.word_id.to_lowercase().contains(&search_lower)
        })
        .collect();

    let visible_rows = matches.len().min(8) as u16;
    let width = 52u16.min(area.width.saturating_sub(4));
    // search row + separator + entries + border
    let height = (visible_rows + 4).min(area.height.saturating_sub(2));
    let x = area.width.saturating_sub(width) / 2;
    let y = area.height.saturating_sub(height) / 2;
    let overlay_area = Rect::new(x, y, width, height);

    f.render_widget(Clear, overlay_area);

    let inner_width = width.saturating_sub(2) as usize;

    let mut lines = vec![];

    // Search line
    lines.push(Line::from(vec![
        Span::styled(" Search: ", Style::default().fg(theme.fg_dim).bg(theme.bg)),
        Span::styled(
            format!("{search}_"),
            Style::default().fg(theme.fg).bg(theme.bg),
        ),
    ]));

    // Separator
    lines.push(Line::from(Span::styled(
        "\u{2500}".repeat(inner_width),
        Style::default().fg(theme.fg_dim).bg(theme.bg),
    )));

    if matches.is_empty() {
        lines.push(Line::from(Span::styled(
            " (no results) ",
            Style::default().fg(theme.fg_dim).bg(theme.bg),
        )));
    } else {
        for (i, entry) in matches.iter().enumerate() {
            if i >= 8 {
                break;
            }
            let is_selected = i == app.session_picker_selected;
            let cursor = if is_selected { ">" } else { " " };
            let name = app.mgr.display_name_for(&entry.meta.word_id);
            let cwd = &entry.meta.cwd;
            let pane_count = entry.panes.len();

            let row_text = format!("{cursor} {name:<20} {pane_count}p  {cwd}");
            let row_text: String = row_text
                .chars()
                .take(inner_width.saturating_sub(1))
                .collect();

            let style = if is_selected {
                Style::default()
                    .fg(theme.bg)
                    .bg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.fg).bg(theme.bg)
            };
            lines.push(Line::from(Span::styled(row_text, style)));
        }
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .title(Span::styled(
            " Sessions ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme.bg));

    f.render_widget(Paragraph::new(lines).block(block), overlay_area);
}

pub(super) fn render_server_picker_overlay(f: &mut Frame, area: Rect, app: &App) {
    let theme = &app.theme;
    let search = &app.server_picker_search;
    let search_lower = search.to_lowercase();

    let servers = app.filtered_servers();
    let visible_rows = servers.len().min(8) as u16;
    let width = 60u16.min(area.width.saturating_sub(4));
    // search row + separator + entries + border
    let height = (visible_rows + 4).min(area.height.saturating_sub(2));
    let x = area.width.saturating_sub(width) / 2;
    let y = area.height.saturating_sub(height) / 2;
    let overlay_area = Rect::new(x, y, width, height);

    f.render_widget(Clear, overlay_area);

    let inner_width = width.saturating_sub(2) as usize;
    let _ = search_lower; // search filtering already done in filtered_servers()

    let mut lines = vec![];

    // Search line
    lines.push(Line::from(vec![
        Span::styled(" Search: ", Style::default().fg(theme.fg_dim).bg(theme.bg)),
        Span::styled(
            format!("{search}_"),
            Style::default().fg(theme.fg).bg(theme.bg),
        ),
    ]));

    // Separator
    lines.push(Line::from(Span::styled(
        "\u{2500}".repeat(inner_width),
        Style::default().fg(theme.fg_dim).bg(theme.bg),
    )));

    if servers.is_empty() {
        lines.push(Line::from(Span::styled(
            " (no recent servers) ",
            Style::default().fg(theme.fg_dim).bg(theme.bg),
        )));
    } else {
        for (i, server) in servers.iter().enumerate() {
            if i >= 8 {
                break;
            }
            let is_selected = i == app.server_picker_selected;
            let cursor = if is_selected { ">" } else { " " };
            let session_count = server.sessions.len();
            let time_ago = server.time_ago();

            let row_text = format!(
                "{cursor} {:<28} {}s  {}",
                server.display, session_count, time_ago
            );
            let row_text: String = row_text
                .chars()
                .take(inner_width.saturating_sub(1))
                .collect();

            let style = if is_selected {
                Style::default()
                    .fg(theme.bg)
                    .bg(theme.purple)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.fg).bg(theme.bg)
            };
            lines.push(Line::from(Span::styled(row_text, style)));
        }
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.purple))
        .title(Span::styled(
            " Servers ",
            Style::default()
                .fg(theme.purple)
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme.bg));

    f.render_widget(Paragraph::new(lines).block(block), overlay_area);
}

pub(super) fn render_dir_picker_overlay(f: &mut Frame, area: Rect, app: &App) {
    let theme = &app.theme;
    let buffer = &app.dir_picker_buffer;

    let matches = app.dir_picker_matches();
    let selected = app.dir_picker_selected.min(matches.len().saturating_sub(1));

    let visible_rows = matches.len().min(6) as u16;
    let width = 60u16.min(area.width.saturating_sub(4));
    // input row + separator + entries + border
    let height = (visible_rows + 5).min(area.height.saturating_sub(2));
    let x = area.width.saturating_sub(width) / 2;
    let y = area.height.saturating_sub(height) / 2;
    let overlay_area = Rect::new(x, y, width, height);

    f.render_widget(Clear, overlay_area);

    let inner_width = width.saturating_sub(2) as usize;

    let mut lines = vec![];

    // Input line
    lines.push(Line::from(vec![
        Span::styled(
            " Directory: ",
            Style::default().fg(theme.fg_dim).bg(theme.bg),
        ),
        Span::styled(
            format!("{buffer}_"),
            Style::default().fg(theme.fg).bg(theme.bg),
        ),
    ]));

    // Separator
    lines.push(Line::from(Span::styled(
        "\u{2500}".repeat(inner_width),
        Style::default().fg(theme.fg_dim).bg(theme.bg),
    )));

    if matches.is_empty() {
        lines.push(Line::from(Span::styled(
            " (no existing sessions — Enter to create new) ",
            Style::default().fg(theme.fg_dim).bg(theme.bg),
        )));
    } else {
        for (i, entry) in matches.iter().take(6).enumerate() {
            let name = app.mgr.display_name_for(&entry.meta.word_id);
            let cwd = &entry.meta.cwd;
            let marker = if i == selected { ">" } else { " " };
            let row_text = format!("{marker} {name:<16} {cwd}");
            let row_text: String = row_text
                .chars()
                .take(inner_width.saturating_sub(1))
                .collect();
            lines.push(Line::from(Span::styled(
                row_text,
                Style::default().fg(theme.fg).bg(theme.bg),
            )));
        }
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .title(Span::styled(
            " Open Session ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme.bg));

    f.render_widget(Paragraph::new(lines).block(block), overlay_area);
}

pub(super) fn render_help_overlay(f: &mut Frame, area: Rect, theme: &Theme) {
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
            .fg(theme.accent)
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
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )));
        } else {
            lines.push(Line::from(vec![
                Span::styled(format!("{:>14}", key), Style::default().fg(theme.green)),
                Span::styled(format!("  {}", desc), Style::default().fg(theme.fg)),
            ]));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Press any key to close ",
        Style::default().fg(theme.fg_dim),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .style(Style::default().bg(theme.bg));

    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        overlay_area,
    );
}

pub(super) fn render_confirm_overlay(f: &mut Frame, area: Rect, session: &str, theme: &Theme) {
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
                Style::default().fg(theme.red).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("'{session}'"), Style::default().fg(theme.fg)),
            Span::styled("? (y/n)", Style::default().fg(theme.fg_dim)),
        ]),
        Line::from(""),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.red))
        .style(Style::default().bg(theme.bg));

    f.render_widget(Paragraph::new(lines).block(block), overlay_area);
}

pub(super) fn render_rename_overlay(
    f: &mut Frame,
    area: Rect,
    session: &str,
    buffer: &str,
    theme: &Theme,
) {
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
                .fg(theme.orange)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(buffer, Style::default().fg(theme.fg))),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.orange))
        .style(Style::default().bg(theme.bg));

    f.render_widget(Paragraph::new(lines).block(block), overlay_area);

    // Cursor in rename field
    let cursor_x = x + 1 + buffer.len() as u16;
    let cursor_y = y + 3;
    if cursor_x < x + width - 1 {
        f.set_cursor_position((cursor_x, cursor_y));
    }
}

pub(super) fn render_hud(f: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let snap = app.mgr.metrics.snapshot(app.force_snapshot_mode);
    let c = &snap.counters;

    let lines = vec![
        Line::from(Span::styled(
            format!(
                "Net+Apply: {:.1}ms avg / {:.1}ms max",
                snap.net_apply_avg_ms, snap.net_apply_max_ms
            ),
            Style::default().fg(theme.green),
        )),
        Line::from(Span::styled(
            format!("Apply:     {:.2}ms avg", snap.apply_avg_ms),
            Style::default().fg(theme.green),
        )),
        Line::from(Span::styled(
            format!("Batch:     {:.1} msgs avg", snap.batch_avg),
            Style::default().fg(theme.green),
        )),
        Line::from(Span::styled(
            format!("Diff:      {} ops", snap.last_diff_ops),
            Style::default().fg(theme.green),
        )),
        Line::from(Span::styled(
            format!("LargeDiff: {:.1}ms", snap.last_large_diff_ms),
            Style::default().fg(if snap.last_large_diff_ms > 16.0 {
                theme.yellow
            } else {
                theme.green
            }),
        )),
        Line::from(Span::styled(
            format!(
                "Snapshot:  {}",
                if snap.snapshot_mode { "FORCED" } else { "off" }
            ),
            Style::default().fg(if snap.snapshot_mode {
                theme.yellow
            } else {
                theme.green
            }),
        )),
        Line::from(Span::styled(
            format!(
                "Disc:{}  Gap:{}  Lag:{}  Sync:{}",
                c.stale_discards, c.seqno_gaps, c.lag_events, c.resyncs
            ),
            Style::default().fg(theme.yellow),
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
        .border_style(Style::default().fg(theme.fg_dim))
        .title(" HUD ")
        .title_style(
            Style::default()
                .fg(theme.green)
                .add_modifier(Modifier::BOLD),
        )
        .style(Style::default().bg(Color::Rgb(0x1a, 0x1d, 0x23)));

    f.render_widget(Paragraph::new(lines).block(block), hud_area);
}
