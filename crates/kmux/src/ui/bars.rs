use kmux_client::connection_state::ConnectionState;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, TopBarAction, TopBarHits};
use crate::mode::{self, Mode};
use crate::theme::Theme;

/// One visible run of the top bar. Segments are separated on screen by a
/// single padding column that is not itself clickable.
struct TopBarSegment {
    span: Span<'static>,
    /// `None` means the segment is inert (e.g. the " — " placeholder shown
    /// when there are no panes).
    action: Option<TopBarAction>,
}

impl TopBarSegment {
    fn new(text: String, style: Style, action: Option<TopBarAction>) -> Self {
        Self {
            span: Span::styled(text, style),
            action,
        }
    }
}

pub(super) fn render_session_bar(f: &mut Frame, app: &mut App, area: Rect) {
    let segments = build_session_bar_segments(app);
    let sep_style = Style::default().bg(app.theme.status_bg);
    app.top_bar_hits = render_segments(f, area, segments, sep_style);
}

/// Produce the segments for the session (top) bar in left-to-right order.
///
/// Pure over `app` — no frame access, no mutation — so it can be unit-tested
/// and so the layout primitive below can measure each segment before drawing.
fn build_session_bar_segments(app: &App) -> Vec<TopBarSegment> {
    let theme = &app.theme;
    let mut segs = Vec::new();

    // Server badge — opens the server picker.
    segs.push(TopBarSegment::new(
        format!(" {} ", app.server_display),
        Style::default()
            .fg(theme.bg)
            .bg(theme.purple)
            .add_modifier(Modifier::BOLD),
        Some(TopBarAction::OpenServerPicker),
    ));

    // Connection state badge — forces a reconnect when clicked.
    let state = app.mgr.connection_state();
    let (badge_bg, label) = connection_badge_style(state, theme);
    segs.push(TopBarSegment::new(
        format!(" {label} "),
        Style::default()
            .fg(theme.bg)
            .bg(badge_bg)
            .add_modifier(Modifier::BOLD),
        Some(TopBarAction::Reconnect),
    ));

    // Session badge — opens the session picker.
    let session_text = if let Some(word_id) = app.mgr.active_session() {
        format!(" \u{25b6} {} ", app.mgr.display_name_for(word_id))
    } else {
        " No sessions ".to_string()
    };
    segs.push(TopBarSegment::new(
        session_text,
        Style::default()
            .fg(theme.bg)
            .bg(theme.accent)
            .add_modifier(Modifier::BOLD),
        Some(TopBarAction::OpenSessionPicker),
    ));

    // Pane tabs — one per pane, each clickable to select that pane.
    let active_pane = app.mgr.active_pane_id().map(|s| s.to_string());
    let panes = app.mgr.active_session_panes();
    if panes.is_empty() {
        segs.push(TopBarSegment::new(
            " \u{2014} ".to_string(),
            Style::default().fg(theme.fg_dim).bg(theme.status_bg),
            None,
        ));
    } else {
        for pane in panes.iter() {
            let is_active = active_pane.as_deref() == Some(pane.pane_id.as_str());
            let label = if is_active {
                format!(" \u{2022}{} ", pane.pane_index)
            } else {
                format!(" {} ", pane.pane_index)
            };
            let style = if is_active {
                Style::default()
                    .fg(theme.bg)
                    .bg(theme.green)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.fg).bg(theme.status_bg)
            };
            segs.push(TopBarSegment::new(
                label,
                style,
                Some(TopBarAction::SelectPane(pane.pane_id.clone())),
            ));
        }
    }

    segs
}

/// Walk `segments` left-to-right, rendering each as a span interleaved with a
/// one-column separator, and return the hit-box each clickable segment
/// occupies. Uses `Span::width()` (unicode-aware) so multi-byte glyphs like
/// `▶` take 1 column, not 3.
fn render_segments(
    f: &mut Frame,
    area: Rect,
    segments: Vec<TopBarSegment>,
    separator_style: Style,
) -> TopBarHits {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(segments.len() * 2);
    let mut regions: Vec<(std::ops::Range<u16>, TopBarAction)> = Vec::new();
    let mut col: u16 = 0;

    for (i, seg) in segments.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" ", separator_style));
            col = col.saturating_add(1);
        }
        let width = seg.span.width() as u16;
        if let Some(action) = seg.action.clone() {
            regions.push((col..col + width, action));
        }
        spans.push(seg.span);
        col = col.saturating_add(width);
    }

    if col < area.width {
        spans.push(Span::styled(
            " ".repeat((area.width - col) as usize),
            separator_style,
        ));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
    TopBarHits { regions }
}

pub(super) fn render_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let mut spans = Vec::new();

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

    spans.push(Span::styled(
        format!("{} sessions", app.mgr.session_list().len()),
        Style::default().fg(theme.fg).bg(theme.status_bg),
    ));

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

    let status_msg = app.mgr.status_msg();
    if !status_msg.is_empty() {
        let used: usize = spans.iter().map(|s| s.width()).sum();
        let msg_len = status_msg.chars().count() + 2;
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

    let used: usize = spans.iter().map(|s| s.width()).sum();
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

    let used: usize = spans.iter().map(|s| s.width()).sum();
    if used < area.width as usize {
        spans.push(Span::styled(
            " ".repeat(area.width as usize - used),
            Style::default().bg(theme.bg),
        ));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

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
        Mode::Connecting { .. } => theme.yellow,
        Mode::Disconnected { .. } => theme.red,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;

    fn seg(text: &str, action: Option<TopBarAction>) -> TopBarSegment {
        TopBarSegment::new(text.to_string(), Style::default(), action)
    }

    /// Render `segments` into a 120-col frame and return the hit-box list.
    /// Wraps the render primitive so tests can exercise layout without
    /// constructing a full `App`.
    fn layout(segments: Vec<TopBarSegment>) -> Vec<(std::ops::Range<u16>, TopBarAction)> {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(120, 1);
        let mut term = Terminal::new(backend).unwrap();
        let mut captured = Vec::new();
        term.draw(|f| {
            let area = f.area();
            let hits = render_segments(f, area, segments, Style::default());
            captured = hits.regions;
        })
        .unwrap();
        captured
    }

    #[test]
    fn empty_segments_produce_no_regions() {
        assert!(layout(vec![]).is_empty());
    }

    #[test]
    fn single_ascii_segment_occupies_its_char_range() {
        let regions = layout(vec![seg(
            " localhost ",
            Some(TopBarAction::OpenServerPicker),
        )]);
        assert_eq!(regions.len(), 1);
        let (r, a) = &regions[0];
        assert_eq!(*a, TopBarAction::OpenServerPicker);
        assert_eq!(*r, 0..11);
    }

    /// Regression: a segment containing `▶` (U+25B6, 3 bytes, 1 column) used
    /// to be measured by byte length, which inflated its hit-box and stole
    /// clicks from the pane tabs to its right.
    #[test]
    fn multibyte_glyph_counts_as_one_column_not_three_bytes() {
        let regions = layout(vec![seg(
            " \u{25b6} main ",
            Some(TopBarAction::OpenSessionPicker),
        )]);
        let (r, _) = &regions[0];
        // " ▶ main " is 8 display columns.
        assert_eq!(r.end - r.start, 8);
    }

    #[test]
    fn separator_column_is_not_clickable() {
        let regions = layout(vec![
            seg(" A ", Some(TopBarAction::OpenServerPicker)),
            seg(" B ", Some(TopBarAction::OpenSessionPicker)),
        ]);
        assert_eq!(regions.len(), 2);
        let (a, _) = &regions[0];
        let (b, _) = &regions[1];
        assert_eq!(a.end, 3);
        assert_eq!(b.start, 4, "must leave a one-column separator gap");
    }

    /// The canonical top-bar layout — the specific case that prompted this
    /// rewrite. Verify pane tabs are reachable by mouse.
    #[test]
    fn pane_tabs_are_clickable_after_session_badge_with_multibyte_glyph() {
        let regions = layout(vec![
            seg(" localhost ", Some(TopBarAction::OpenServerPicker)),
            seg(" CONNECTED \u{00b7} UDS ", Some(TopBarAction::Reconnect)),
            seg(" \u{25b6} main ", Some(TopBarAction::OpenSessionPicker)),
            seg(" \u{2022}0 ", Some(TopBarAction::SelectPane("p0".into()))),
            seg(" 1 ", Some(TopBarAction::SelectPane("p1".into()))),
        ]);

        let hits = TopBarHits { regions };
        let session = hits
            .regions
            .iter()
            .find(|(_, a)| matches!(a, TopBarAction::OpenSessionPicker))
            .unwrap();
        let first_pane = hits
            .regions
            .iter()
            .find(|(_, a)| matches!(a, TopBarAction::SelectPane(id) if id == "p0"))
            .unwrap();

        assert!(
            first_pane.0.start > session.0.end,
            "pane tab must not overlap session badge: session={:?} pane0={:?}",
            session.0,
            first_pane.0,
        );

        // A click one column inside the first pane tab resolves to that pane.
        let col = first_pane.0.start;
        let action = hits.action_at(col).unwrap();
        assert!(matches!(action, TopBarAction::SelectPane(id) if id == "p0"));

        // A click inside the session badge still opens the session picker,
        // not the pane selector.
        let col = session.0.start + 1;
        assert!(matches!(
            hits.action_at(col).unwrap(),
            TopBarAction::OpenSessionPicker
        ));
    }

    #[test]
    fn inert_segment_contributes_layout_width_but_no_hitbox() {
        // " — " (em dash) placeholder when there are no panes.
        let regions = layout(vec![
            seg(" \u{25b6} ", Some(TopBarAction::OpenSessionPicker)),
            seg(" \u{2014} ", None),
        ]);
        assert_eq!(
            regions.len(),
            1,
            "inert segment must not register a hit-box"
        );
        let (session, _) = &regions[0];
        assert_eq!(session.end, 3);
    }
}
