//! Metrics overlay: per-transport bytes/msgs + RTT summary + render stats.
//!
//! Toggled via `Ctrl+G` then `m`. Reads purely from `SessionManager::metrics`;
//! does not reach into the filesystem. For historical cross-session data,
//! the caller can read the rolling JSONL via
//! [`kmux_client::metrics::JsonlSink::read_history`].

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use kmux_client::metrics::{MetricsStore, TransportCounters, TransportKey};
use kmux_protocol::messages::{MessageCategory, TransportKind};

use crate::app::App;
use crate::theme::Theme;

use super::centered_overlay;

pub fn render_metrics_overlay(f: &mut Frame, area: Rect, app: &App) {
    let theme = &app.theme;
    let metrics = &app.mgr.metrics;

    let width = 72u16.min(area.width.saturating_sub(4));
    let height = 36u16.min(area.height.saturating_sub(2));
    let overlay_area = centered_overlay(area, width, height);

    f.render_widget(Clear, overlay_area);

    let mut lines: Vec<Line> = Vec::new();

    // Header row: process identity.
    lines.push(Line::from(vec![Span::styled(
        format!(
            " kmux pid={} conn={}",
            std::process::id(),
            app.mgr
                .connection_id
                .map(|c| c.0.to_string())
                .unwrap_or_else(|| "-".into()),
        ),
        Style::default().fg(theme.fg_dim),
    )]));

    let sink_status = match metrics.sink_path() {
        Some(p) => format!(" sink: {}", p.display()),
        None => " sink: (disabled)".to_string(),
    };
    lines.push(Line::from(Span::styled(
        sink_status,
        Style::default().fg(theme.fg_dim),
    )));
    lines.push(Line::from(""));

    // Per-transport breakdown.
    let active = metrics.active_transport().cloned();

    // Collect per-transport totals for card headers.
    let mut by_transport = metrics.network.snapshot_by_transport();
    // Stable order: by kind first, then by address so same-kind transports
    // don't shuffle with HashMap iteration order on each frame.
    by_transport.sort_by(|a, b| {
        transport_sort_order(a.0.kind)
            .cmp(&transport_sort_order(b.0.kind))
            .then_with(|| a.0.address.cmp(&b.0.address))
    });

    // Collect per-category breakdown for sub-rows.
    let all_buckets = metrics.network.snapshot();

    if by_transport.is_empty() {
        lines.push(Line::from(Span::styled(
            " (no transport traffic yet)",
            Style::default().fg(theme.fg_dim),
        )));
    } else {
        for (key, totals) in &by_transport {
            let is_active = active.as_ref().map(|a| a == key).unwrap_or(false);
            let title_color = if is_active { theme.green } else { theme.fg };
            let marker = if is_active { "●" } else { " " };

            // Transport card header with aggregate totals.
            lines.push(Line::from(vec![
                Span::styled(format!(" {marker} "), Style::default().fg(theme.green)),
                Span::styled(
                    format!("{} {}", key.kind, key.address),
                    Style::default()
                        .fg(title_color)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::styled("     ", Style::default()),
                Span::styled(
                    format!(
                        "total  in {}  out {}   msgs {}/{}",
                        fmt_bytes(totals.bytes_in),
                        fmt_bytes(totals.bytes_out),
                        totals.msgs_in,
                        totals.msgs_out,
                    ),
                    Style::default().fg(theme.fg),
                ),
            ]));

            // Per-category sub-rows (non-zero only, stable order).
            let mut cat_rows: Vec<(MessageCategory, &TransportCounters)> = all_buckets
                .iter()
                .filter(|(k, _, _)| k == key)
                .map(|(_, cat, c)| (*cat, c))
                .collect();
            cat_rows.sort_by_key(|(cat, _)| cat.as_sort_key());

            for (cat, c) in &cat_rows {
                lines.push(category_line(*cat, c, theme));
            }

            lines.push(rtt_line(metrics, key, theme));
            lines.push(Line::from(""));
        }
    }

    // Apply-side render stats (mirrors HUD but without snapshot toggle).
    let snap = metrics.snapshot(app.force_snapshot_mode);
    lines.push(Line::from(Span::styled(
        " Render",
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(vec![
        Span::styled("   ", Style::default()),
        Span::styled(
            format!(
                "net+apply {:.1}ms avg / {:.1}ms max   apply {:.2}ms avg   batch {:.1}",
                snap.net_apply_avg_ms, snap.net_apply_max_ms, snap.apply_avg_ms, snap.batch_avg,
            ),
            Style::default().fg(theme.fg),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("   ", Style::default()),
        Span::styled(
            format!(
                "disc {}  gap {}  lag {}  resync {}",
                snap.counters.stale_discards,
                snap.counters.seqno_gaps,
                snap.counters.lag_events,
                snap.counters.resyncs,
            ),
            Style::default().fg(theme.yellow),
        ),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Ctrl+G m to close",
        Style::default().fg(theme.fg_dim),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .title(Span::styled(
            " Metrics ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(Color::Rgb(0x1a, 0x1d, 0x23)));

    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        overlay_area,
    );
}

fn category_line<'a>(cat: MessageCategory, c: &TransportCounters, theme: &Theme) -> Line<'a> {
    Line::from(vec![
        Span::styled("     ", Style::default()),
        Span::styled(
            format!(
                "{:<10} in {}  out {}   msgs {}/{}",
                cat.to_string(),
                fmt_bytes(c.bytes_in),
                fmt_bytes(c.bytes_out),
                c.msgs_in,
                c.msgs_out,
            ),
            Style::default().fg(theme.fg_dim),
        ),
    ])
}

fn rtt_line(metrics: &MetricsStore, key: &TransportKey, theme: &Theme) -> Line<'static> {
    let summary = metrics.rtt.summary(key);
    let body = match summary {
        Some(s) if s.sample_count > 0 => {
            let ewma = s.ewma_ms.unwrap_or(0.0);
            format!(
                "rtt    ewma {:.1}ms  recent avg {:.1}ms  max {:.1}ms  n={}",
                ewma, s.recent_avg_ms, s.recent_max_ms, s.sample_count,
            )
        }
        _ => "rtt    (no samples yet)".to_string(),
    };
    Line::from(vec![
        Span::styled("     ", Style::default()),
        Span::styled(body, Style::default().fg(theme.fg)),
    ])
}

fn fmt_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if n >= GB {
        format!("{:.2} GiB", n as f64 / GB as f64)
    } else if n >= MB {
        format!("{:.2} MiB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1} KiB", n as f64 / KB as f64)
    } else {
        format!("{n} B")
    }
}

/// Stable display order: UDS first (fastest), then QUIC, then TCP+TLS, then
/// plain TCP. Matches the scorer's robustness ordering.
fn transport_sort_order(kind: TransportKind) -> u8 {
    match kind {
        TransportKind::Uds => 0,
        TransportKind::Quic => 1,
        TransportKind::TcpTls => 2,
        TransportKind::Tcp => 3,
    }
}

#[cfg(test)]
mod tests {
    use kmux_client::metrics::TransportCounters;
    use kmux_protocol::messages::{MessageCategory, TransportKind};

    use super::*;

    #[test]
    fn fmt_bytes_scales_units() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(2048), "2.0 KiB");
        assert!(fmt_bytes(5 * 1024 * 1024).ends_with("MiB"));
    }

    #[test]
    fn sort_order_prefers_uds_then_quic() {
        assert!(
            transport_sort_order(TransportKind::Uds) < transport_sort_order(TransportKind::Quic)
        );
        assert!(
            transport_sort_order(TransportKind::Quic) < transport_sort_order(TransportKind::TcpTls)
        );
    }

    #[test]
    fn same_kind_transports_sort_deterministically_by_address() {
        use kmux_client::metrics::TransportKey;

        // Two same-kind entries: the overlay sort must pin them by address,
        // otherwise HashMap iteration order causes them to swap each frame.
        let mut entries = [
            (
                TransportKey::new(TransportKind::Quic, "zeta.example:8443"),
                TransportCounters::default(),
            ),
            (
                TransportKey::new(TransportKind::Quic, "alpha.example:8443"),
                TransportCounters::default(),
            ),
            (
                TransportKey::new(TransportKind::Uds, "/tmp/b.sock"),
                TransportCounters::default(),
            ),
            (
                TransportKey::new(TransportKind::Uds, "/tmp/a.sock"),
                TransportCounters::default(),
            ),
        ];
        entries.sort_by(|a, b| {
            transport_sort_order(a.0.kind)
                .cmp(&transport_sort_order(b.0.kind))
                .then_with(|| a.0.address.cmp(&b.0.address))
        });
        let addrs: Vec<&str> = entries.iter().map(|(k, _)| k.address.as_str()).collect();
        assert_eq!(
            addrs,
            vec![
                "/tmp/a.sock",
                "/tmp/b.sock",
                "alpha.example:8443",
                "zeta.example:8443",
            ],
        );
    }

    #[test]
    fn category_line_renders_nonzero_bucket() {
        let theme = crate::theme::default_theme();
        let counters = TransportCounters {
            bytes_in: 1024,
            bytes_out: 512,
            msgs_in: 4,
            msgs_out: 2,
        };
        let line = category_line(MessageCategory::Shell, &counters, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("Shell"));
        assert!(text.contains("1.0 KiB"));
        assert!(text.contains("512 B"));
    }
}
