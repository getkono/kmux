use iced::{
    Color as IcedColor, Font, Pixels, Point as IcedPoint, Rectangle, Size, alignment,
    widget::canvas::{self, Text},
};

use kmux_client::event_log::DiagSnapshot;
use kmux_client::metrics::MetricsSnapshot;

/// Draw a scroll position indicator at the top-right corner.
pub fn draw_scroll_indicator(
    renderer: &iced::Renderer,
    bounds: Rectangle,
    scroll_offset: usize,
    scrollback_len: usize,
) -> canvas::Geometry {
    let mut frame = canvas::Frame::new(renderer, bounds.size());

    let label = format!("[{}/{}]", scroll_offset, scrollback_len);
    let pad = 8.0;
    let font_size = 12.0;
    // Approximate text width: ~7px per character at size 12.
    let text_w = label.len() as f32 * 7.0;
    let x = bounds.width - text_w - pad;
    let y = pad;

    // Semi-transparent background pill.
    frame.fill_rectangle(
        IcedPoint::new(x - 4.0, y - 2.0),
        Size::new(text_w + 8.0, 18.0),
        IcedColor {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.7,
        },
    );

    frame.fill_text(Text {
        content: label,
        position: IcedPoint::new(x, y),
        color: IcedColor::from_rgb8(0xf1, 0xfa, 0x8c), // amber
        size: Pixels(font_size),
        line_height: iced::widget::text::LineHeight::Absolute(Pixels(16.0)),
        font: Font::MONOSPACE,
        horizontal_alignment: alignment::Horizontal::Left,
        vertical_alignment: alignment::Vertical::Top,
        shaping: iced::widget::text::Shaping::Basic,
    });

    frame.into_geometry()
}

/// Draw the HUD overlay as an uncached geometry layer.
pub fn draw_hud(
    renderer: &iced::Renderer,
    bounds: Rectangle,
    metrics: &MetricsSnapshot,
    diag: Option<&DiagSnapshot>,
    draw_ms: f64,
    fps: f64,
    cache_hit: bool,
) -> canvas::Geometry {
    const HUD_W: f32 = 320.0;
    const HUD_PAD: f32 = 8.0;
    const LINE_H: f32 = 18.0;
    const HUD_FONT_SIZE: f32 = 12.0;
    const MAX_EVENTS: usize = 5;

    let mut frame = canvas::Frame::new(renderer, bounds.size());

    let hud_x = bounds.width - HUD_W - HUD_PAD;
    let hud_y = HUD_PAD;

    let cache_label = if cache_hit {
        "HIT (overlay)"
    } else {
        "MISS (rebuild)"
    };
    let green = IcedColor::from_rgb8(0x50, 0xfa, 0x7b);
    let amber = IcedColor::from_rgb8(0xf1, 0xfa, 0x8c);
    let dim = IcedColor::from_rgb8(0x88, 0x88, 0x88);

    // Collect all HUD lines with their colors
    let c = &metrics.counters;
    let mut lines: Vec<(String, IcedColor)> = vec![
        (
            format!(
                "Net+Apply: {:.1}ms avg / {:.1}ms max",
                metrics.net_apply_avg_ms, metrics.net_apply_max_ms
            ),
            green,
        ),
        (
            format!("Apply:     {:.2}ms avg", metrics.apply_avg_ms),
            green,
        ),
        (format!("Draw:      {:.1}ms (prev frame)", draw_ms), green),
        (
            format!("Batch:     {:.1} msgs avg", metrics.batch_avg),
            green,
        ),
        (format!("FPS:       {fps:.0}"), green),
        (format!("Diff:      {} ops", metrics.last_diff_ops), green),
        (
            format!("LargeDiff: {:.1}ms", metrics.last_large_diff_ms),
            if metrics.last_large_diff_ms > 16.0 {
                amber
            } else {
                green
            },
        ),
        (format!("Cache:     {cache_label}"), green),
        (
            format!(
                "Snapshot:  {}",
                if metrics.snapshot_mode {
                    "FORCED"
                } else {
                    "off"
                }
            ),
            if metrics.snapshot_mode { amber } else { green },
        ),
        ("--- Diagnostics ---".to_owned(), dim),
        (
            format!(
                "Disc:{}  Gap:{}  Lag:{}  Sync:{}",
                c.stale_discards, c.seqno_gaps, c.lag_events, c.resyncs
            ),
            amber,
        ),
    ];

    // Recent events
    if let Some(diag) = diag {
        for (ts, text) in diag.events.iter().rev().take(MAX_EVENTS).rev() {
            let ago = ts.elapsed().as_secs();
            let label = if ago < 60 {
                format!("[{ago}s ago] {text}")
            } else {
                format!("[{}m ago] {text}", ago / 60)
            };
            lines.push((label, amber));
        }
    }

    // Semi-transparent background sized to actual content
    let hud_h = 6.0 + lines.len() as f32 * LINE_H + 6.0;
    frame.fill_rectangle(
        IcedPoint::new(hud_x, hud_y),
        Size::new(HUD_W, hud_h),
        IcedColor {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.75,
        },
    );

    for (i, (content, color)) in lines.iter().enumerate() {
        frame.fill_text(Text {
            content: content.clone(),
            position: IcedPoint::new(hud_x + 8.0, hud_y + 6.0 + i as f32 * LINE_H),
            color: *color,
            size: Pixels(HUD_FONT_SIZE),
            line_height: iced::widget::text::LineHeight::Absolute(Pixels(LINE_H)),
            font: Font::MONOSPACE,
            horizontal_alignment: alignment::Horizontal::Left,
            vertical_alignment: alignment::Vertical::Top,
            shaping: iced::widget::text::Shaping::Basic,
        });
    }

    frame.into_geometry()
}
