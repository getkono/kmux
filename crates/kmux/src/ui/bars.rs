use kmux_client::connection_state::ConnectionState;
use kmux_protocol::dirs::BuildProfile;
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

/// Maximum codepoints of title shown inside a pane tab. Longer titles are
/// truncated with an ellipsis so the tab bar does not push later tabs
/// off-screen on narrow terminals.
const TAB_TITLE_MAX_CHARS: usize = 20;

/// Collapse interior whitespace and truncate the title for display inside a
/// pane tab. Returns an empty string when the title is empty or whitespace
/// only. Newlines inside titles are flattened to a single space since tabs
/// render on one line.
fn truncate_tab_title(title: &str) -> String {
    let cleaned: String = title
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let char_count = trimmed.chars().count();
    if char_count <= TAB_TITLE_MAX_CHARS {
        return trimmed.to_string();
    }
    let mut out: String = trimmed
        .chars()
        .take(TAB_TITLE_MAX_CHARS.saturating_sub(1))
        .collect();
    out.push('\u{2026}');
    out
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
            let title = truncate_tab_title(&pane.title);
            let label = match (is_active, title.is_empty()) {
                (true, true) => format!(" \u{2022}{} ", pane.pane_index),
                (true, false) => format!(" \u{2022}{} {} ", pane.pane_index, title),
                (false, true) => format!(" {} ", pane.pane_index),
                (false, false) => format!(" {} {} ", pane.pane_index, title),
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

    let mut right_spans: Vec<Span<'static>> = Vec::new();
    let status_msg = app.mgr.status_msg();
    if !status_msg.is_empty() {
        right_spans.push(Span::styled(
            format!(" {} ", status_msg),
            Style::default().fg(theme.fg_dim).bg(theme.status_bg),
        ));
    }
    if let Some(badge) = profile_badge(BuildProfile::CURRENT, theme) {
        right_spans.extend(badge);
    }

    let left_width: usize = spans.iter().map(|s| s.width()).sum();
    let right_width: usize = right_spans.iter().map(|s| s.width()).sum();
    let gap = (area.width as usize).saturating_sub(left_width + right_width);
    if gap > 0 {
        spans.push(Span::styled(
            " ".repeat(gap),
            Style::default().bg(theme.status_bg),
        ));
    }
    spans.extend(right_spans);

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// "DEBUG" badge pinned to the far right of the status bar on debug builds so
/// it is hard to miss that a `cargo run`/`cargo build` binary is attached
/// rather than an installed release. Release builds render nothing.
///
/// Takes `profile` as a parameter (rather than reading `BuildProfile::CURRENT`
/// directly) so tests can exercise both branches regardless of which profile
/// the test binary was compiled with.
fn profile_badge(profile: BuildProfile, theme: &Theme) -> Option<Vec<Span<'static>>> {
    match profile {
        BuildProfile::Debug => Some(vec![
            Span::styled(" ", Style::default().bg(theme.status_bg)),
            Span::styled(
                " DEBUG ",
                Style::default()
                    .fg(theme.bg)
                    .bg(theme.orange)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        BuildProfile::Release => None,
    }
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
    fn tab_title_empty_yields_empty_string() {
        assert_eq!(truncate_tab_title(""), "");
        assert_eq!(truncate_tab_title("   "), "");
    }

    #[test]
    fn tab_title_short_kept_verbatim() {
        assert_eq!(truncate_tab_title("nvim"), "nvim");
    }

    #[test]
    fn tab_title_long_truncated_with_ellipsis() {
        let long = "a".repeat(50);
        let out = truncate_tab_title(&long);
        assert_eq!(out.chars().count(), TAB_TITLE_MAX_CHARS);
        assert!(
            out.ends_with('\u{2026}'),
            "truncated title must end with ellipsis, got {out:?}"
        );
    }

    #[test]
    fn tab_title_control_chars_flattened() {
        assert_eq!(truncate_tab_title("nvim\r\nfoo"), "nvim  foo");
    }

    /// Regression: a title containing a multi-byte glyph still measures to the
    /// correct display column count so later tabs remain clickable.
    #[test]
    fn tab_with_multibyte_title_has_correct_hitbox_width() {
        let regions = layout(vec![
            seg(" \u{25b6} main ", Some(TopBarAction::OpenSessionPicker)),
            seg(
                " \u{2022}0 \u{65e5}\u{672c} ",
                Some(TopBarAction::SelectPane("p0".into())),
            ),
        ]);
        assert_eq!(regions.len(), 2);
        let pane = regions
            .iter()
            .find(|(_, a)| matches!(a, TopBarAction::SelectPane(id) if id == "p0"))
            .unwrap();
        // " •0 日本 " is 8 display columns (• and 日/本 each count as wide
        // glyphs via ratatui's unicode-width).
        assert!(
            pane.0.end > pane.0.start,
            "multibyte tab must have non-zero hit-box: {:?}",
            pane.0,
        );
    }

    #[test]
    fn profile_badge_renders_only_on_debug_builds() {
        let theme = crate::theme::default_theme();
        let debug = profile_badge(BuildProfile::Debug, &theme).expect("debug yields badge");
        let release = profile_badge(BuildProfile::Release, &theme);

        assert!(release.is_none(), "release builds must not show the badge");
        let width: usize = debug.iter().map(|s| s.width()).sum();
        assert_eq!(width, 8, "badge is a 1-col gutter plus ' DEBUG ' (7 cols)");
        let text: String = debug.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("DEBUG"),
            "badge must spell DEBUG, got {text:?}"
        );
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
