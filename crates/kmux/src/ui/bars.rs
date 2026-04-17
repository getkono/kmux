use kmux_client::connection_state::ConnectionState;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::mode::{self, Mode};
use crate::theme::Theme;

pub(super) fn render_session_bar(f: &mut Frame, app: &mut App, area: Rect) {
    let theme = &app.theme;
    let mut spans = Vec::new();

    // Far left: server badge (clickable, opens server picker)
    let server_text = format!(" {} ", app.server_display);
    let server_width = server_text.len() as u16;
    app.server_badge_cols = server_width;
    spans.push(Span::styled(
        server_text,
        Style::default()
            .fg(theme.bg)
            .bg(theme.purple)
            .add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(" ", Style::default().bg(theme.status_bg)));

    // Connection status badge — green/yellow/red by state.
    let state = app.mgr.connection_state();
    let (badge_bg, label) = connection_badge_style(state, theme);
    let badge_text = format!(" {label} ");
    spans.push(Span::styled(
        badge_text,
        Style::default()
            .fg(theme.bg)
            .bg(badge_bg)
            .add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(" ", Style::default().bg(theme.status_bg)));

    // Left: session badge (clickable, opens session picker)
    let badge_text = if let Some(word_id) = app.mgr.active_session() {
        let name = app.mgr.display_name_for(word_id);
        format!(" \u{25b6} {name} ")
    } else {
        " No sessions ".to_string()
    };
    let badge_width = badge_text.len() as u16;
    // Store badge width for mouse click detection
    app.session_badge_cols = badge_width;

    spans.push(Span::styled(
        badge_text,
        Style::default()
            .fg(theme.bg)
            .bg(theme.accent)
            .add_modifier(Modifier::BOLD),
    ));

    // Separator
    spans.push(Span::styled(" ", Style::default().bg(theme.status_bg)));

    // Right: pane tabs for the active session
    let active_pane = app.mgr.active_pane_id().map(|s| s.to_string());
    let panes: Vec<_> = app
        .mgr
        .active_session_panes()
        .iter()
        .map(|p| (p.pane_id.clone(), p.pane_index))
        .collect();

    if panes.is_empty() {
        spans.push(Span::styled(
            " — ",
            Style::default().fg(theme.fg_dim).bg(theme.status_bg),
        ));
    } else {
        for (pane_id, pane_index) in &panes {
            let is_active = active_pane.as_deref() == Some(pane_id.as_str());
            let label = if is_active {
                format!(" \u{2022}{pane_index} ")
            } else {
                format!(" {pane_index} ")
            };
            let style = if is_active {
                Style::default()
                    .fg(theme.bg)
                    .bg(theme.green)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.fg).bg(theme.status_bg)
            };
            spans.push(Span::styled(label, style));
            spans.push(Span::styled(" ", Style::default().bg(theme.status_bg)));
        }
    }

    // Pad the rest
    let used: usize = spans.iter().map(|s| s.content.len()).sum();
    if used < area.width as usize {
        spans.push(Span::styled(
            " ".repeat(area.width as usize - used),
            Style::default().bg(theme.status_bg),
        ));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

pub(super) fn render_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let mut spans = Vec::new();

    // Connection info
    let host_port = app.mgr.host_port_display();
    if !host_port.is_empty() {
        spans.push(Span::styled(
            format!(" {} ", host_port),
            Style::default().fg(theme.green).bg(theme.status_bg),
        ));
        spans.push(Span::styled(
            " | ",
            Style::default().fg(theme.fg_dim).bg(theme.status_bg),
        ));
    }

    // Session count
    spans.push(Span::styled(
        format!("{} sessions", app.mgr.session_list().len()),
        Style::default().fg(theme.fg).bg(theme.status_bg),
    ));

    // Input lock
    if app.mgr.active_input_locked() {
        spans.push(Span::styled(
            " | ",
            Style::default().fg(theme.fg_dim).bg(theme.status_bg),
        ));
        spans.push(Span::styled(
            "LOCKED",
            Style::default()
                .fg(theme.red)
                .bg(theme.status_bg)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // Size
    if let Some((rows, cols)) = app.mgr.active_term_size() {
        spans.push(Span::styled(
            " | ",
            Style::default().fg(theme.fg_dim).bg(theme.status_bg),
        ));
        spans.push(Span::styled(
            format!("{cols}x{rows}"),
            Style::default().fg(theme.fg_dim).bg(theme.status_bg),
        ));
    }

    // Status message (right-aligned)
    let status_msg = app.mgr.status_msg();
    if !status_msg.is_empty() {
        let used: usize = spans.iter().map(|s| s.content.len()).sum();
        let msg_len = status_msg.len() + 2;
        let gap = (area.width as usize).saturating_sub(used + msg_len);
        if gap > 0 {
            spans.push(Span::styled(
                " ".repeat(gap),
                Style::default().bg(theme.status_bg),
            ));
        }
        spans.push(Span::styled(
            format!(" {} ", status_msg),
            Style::default().fg(theme.fg_dim).bg(theme.status_bg),
        ));
    }

    // Pad
    let used: usize = spans.iter().map(|s| s.content.len()).sum();
    if used < area.width as usize {
        spans.push(Span::styled(
            " ".repeat(area.width as usize - used),
            Style::default().bg(theme.status_bg),
        ));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

pub(super) fn render_hint_bar(f: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let hints = mode::mode_hints(&app.mode);
    let mode_name = mode::mode_name(&app.mode);

    let mut spans = vec![Span::styled(
        format!(" {} ", mode_name),
        Style::default()
            .fg(theme.bg)
            .bg(mode_color(&app.mode, theme))
            .add_modifier(Modifier::BOLD),
    )];

    for (key, desc) in &hints {
        spans.push(Span::styled(" ", Style::default().bg(theme.bg)));
        spans.push(Span::styled(
            format!(" {key} "),
            Style::default()
                .fg(theme.bg)
                .bg(theme.fg_dim)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {desc}"),
            Style::default().fg(theme.fg).bg(theme.bg),
        ));
    }

    // Pad
    let used: usize = spans.iter().map(|s| s.content.len()).sum();
    if used < area.width as usize {
        spans.push(Span::styled(
            " ".repeat(area.width as usize - used),
            Style::default().bg(theme.bg),
        ));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Map a `ConnectionState` to a (bg color, label) pair for the top-bar badge.
fn connection_badge_style(state: &ConnectionState, theme: &Theme) -> (Color, String) {
    let label = state.badge_label();
    let color = match state {
        ConnectionState::Connected { .. } => theme.green,
        ConnectionState::Handshaking | ConnectionState::Reconnecting { .. } => theme.yellow,
        ConnectionState::Disconnected { .. } => theme.red,
        ConnectionState::Idle => theme.fg_dim,
    };
    (color, label)
}

pub(super) fn mode_color(mode: &Mode, theme: &Theme) -> Color {
    match mode {
        Mode::Normal => theme.green,
        Mode::Locked => theme.red,
        Mode::Select => theme.accent,
        Mode::Session => theme.purple,
        Mode::Scroll => theme.yellow,
        Mode::Signal => theme.red,
        Mode::ConfirmCloseSession { .. } => theme.red,
        Mode::RenameSession { .. } => theme.orange,
        Mode::SessionPicker => theme.accent,
        Mode::ServerPicker => theme.purple,
        Mode::Help => theme.accent,
        Mode::Connect { .. } => theme.accent,
        Mode::DirectoryPicker => theme.accent,
        Mode::Disconnected { .. } => theme.red,
    }
}
