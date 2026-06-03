//! Cairo + Pango rendering of `AppCore`'s active terminal grid.
//!
//! The cell geometry is derived from the configured font ([`Metrics`]) rather
//! than hardcoded, so the grid stays aligned at any font size or display scale.
//! This is the GTK analog of the TUI's `ui/grid.rs` and mirrors its handling of
//! default colors, wide-char spacers, control chars, text attributes, cursor
//! shapes, and scrollback.

use gtk4::cairo;
use gtk4::pango;

use kmux_app::core::AppCore;
use kmux_app::theme::Theme as Palette;
use kmux_client::grid::{ScrollbackBuffer, scrollback_display_row_at};
use kmux_protocol::messages::{CellAttrs, CellState, CursorShape};

/// Cell geometry derived from the configured font. Recomputed when the font or
/// the display scale factor changes.
pub struct Metrics {
    /// Cell advance width in (logical) pixels.
    pub cell_w: f64,
    /// Cell height (ascent + descent) in (logical) pixels.
    pub cell_h: f64,
    /// The font the metrics were measured for; reused to render glyphs.
    pub font: pango::FontDescription,
}

impl Metrics {
    /// Measure cell size for `font` using `ctx` — a widget's `PangoContext`,
    /// which carries the display font map, resolution, and scale factor, so the
    /// result is in the same (logical) pixel space the `DrawingArea` draws in.
    pub fn measure(ctx: &pango::Context, font: pango::FontDescription) -> Self {
        let fm = ctx.metrics(Some(&font), None);
        let line_h = (fm.ascent() + fm.descent()) as f64 / pango::SCALE as f64;
        let cell_h = line_h.ceil().max(1.0);

        // Measure a representative advance for the (monospace) face; ceil so
        // cells tile without sub-pixel seams.
        let layout = pango::Layout::new(ctx);
        layout.set_font_description(Some(&font));
        layout.set_text("M");
        let char_w = layout.size().0 as f64 / pango::SCALE as f64;
        let cell_w = char_w.ceil().max(1.0);

        Self {
            cell_w,
            cell_h,
            font,
        }
    }

    /// Cols/rows that fit a `width_px × height_px` content area.
    pub fn cols_rows(&self, width_px: i32, height_px: i32) -> (u16, u16) {
        let cols = (width_px as f64 / self.cell_w).floor().max(1.0) as u16;
        let rows = (height_px as f64 / self.cell_h).floor().max(1.0) as u16;
        (cols, rows)
    }
}

/// Set the cairo source to an opaque 8-bit RGB triple.
fn src(cr: &cairo::Context, r: u8, g: u8, b: u8) {
    cr.set_source_rgb(r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
}

/// Resolve the cell shown at visible position `(vr, vc)`: a scrollback line when
/// scrolled into history, otherwise the live grid. Mirrors `ui/grid.rs`.
fn cell_at<'a>(
    cells: &'a [CellState],
    scrollback: &'a ScrollbackBuffer,
    scroll_offset: usize,
    cols: usize,
    sb_row: Option<(usize, usize)>,
    vr: usize,
    vc: usize,
) -> Option<&'a CellState> {
    if let Some((line_idx, col_start)) = sb_row {
        scrollback
            .get(line_idx)
            .and_then(|line| line.get(col_start + vc))
    } else if scroll_offset > 0 {
        let grid_row = vr.checked_sub(scroll_offset)?;
        cells.get(grid_row * cols + vc)
    } else {
        cells.get(vr * cols + vc)
    }
}

/// Paint the active grid into `cr`. `width`/`height` are the content area's
/// logical pixel size (used to center the no-session message and place the
/// scroll indicator).
pub fn render(
    core: &AppCore,
    cr: &cairo::Context,
    ctx: &pango::Context,
    metrics: &Metrics,
    width: i32,
    height: i32,
    cursor_phase: bool,
) {
    let palette = &core.palette;
    src(cr, palette.bg.r, palette.bg.g, palette.bg.b);
    let _ = cr.paint();

    let Some(grid) = core.mgr.active_grid() else {
        placeholder(cr, ctx, metrics, palette, width, height);
        return;
    };

    let layout = pango::Layout::new(ctx);
    let cells = grid.cells();
    let cols = grid.cols;
    let rows = grid.rows;
    let scroll_offset = grid.scroll_offset();
    let scrollback = grid.scrollback();

    for vr in 0..rows {
        let sb_row = if scroll_offset > 0 && vr < scroll_offset {
            let rev = scroll_offset - 1 - vr;
            scrollback_display_row_at(scrollback, cols, rev)
        } else {
            None
        };

        // Two phases per row: fill every cell background first, then draw glyphs.
        // A double-width glyph in the lead cell renders over the following
        // spacer cell's area, so the spacer's background must already be down.
        for vc in 0..cols {
            if let Some(cs) = cell_at(cells, scrollback, scroll_offset, cols, sb_row, vr, vc) {
                fill_cell_bg(cr, metrics, palette, cs, vc, vr);
            }
        }
        for vc in 0..cols {
            if let Some(cs) = cell_at(cells, scrollback, scroll_offset, cols, sb_row, vr, vc) {
                draw_cell_glyph(cr, &layout, metrics, palette, cs, vc, vr);
            }
        }
    }

    // Selection wash: a translucent accent tint over selected cells. The spans
    // are computed by `CellGrid` (scroll- and wrap-aware), so the wash paints
    // over scrollback rows too while scrolled into history.
    let spans = grid.visible_selection_spans();
    if !spans.is_empty() {
        let a = palette.accent;
        cr.set_source_rgba(
            a.r as f64 / 255.0,
            a.g as f64 / 255.0,
            a.b as f64 / 255.0,
            0.30,
        );
        for (vr, c0, c1) in spans {
            let x = c0 as f64 * metrics.cell_w;
            let w = (c1 - c0 + 1) as f64 * metrics.cell_w;
            cr.rectangle(x, vr as f64 * metrics.cell_h, w, metrics.cell_h);
            let _ = cr.fill();
        }
    }

    // Cursor only renders against the live screen (not while scrolled back).
    // A blinking cursor (DECSCUSR `blinking_*`) is shown only on the "on" half
    // of the blink cycle; a steady cursor is always shown.
    if scroll_offset == 0 {
        let cursor = grid.cursor();
        if cursor.visible && cursor.shape != CursorShape::Hidden && (!cursor.blink || cursor_phase)
        {
            draw_cursor(cr, &layout, metrics, palette, cells, cols, rows, cursor);
        }
    }

    if scroll_offset > 0 {
        draw_scroll_indicator(
            cr,
            &layout,
            metrics,
            palette,
            scroll_offset,
            grid.total_scrollback_display_rows(),
            width,
        );
    }
}

/// Background fill color for a cell (default-bg attr → palette background).
fn cell_bg(cs: &CellState, palette: &Palette) -> (u8, u8, u8) {
    if cs.attrs.contains(CellAttrs::DEFAULT_BG) {
        (palette.bg.r, palette.bg.g, palette.bg.b)
    } else {
        (cs.bg.r, cs.bg.g, cs.bg.b)
    }
}

fn fill_cell_bg(
    cr: &cairo::Context,
    m: &Metrics,
    palette: &Palette,
    cs: &CellState,
    col: usize,
    row: usize,
) {
    let (r, g, b) = cell_bg(cs, palette);
    src(cr, r, g, b);
    cr.rectangle(
        col as f64 * m.cell_w,
        row as f64 * m.cell_h,
        m.cell_w,
        m.cell_h,
    );
    let _ = cr.fill();
}

fn draw_cell_glyph(
    cr: &cairo::Context,
    layout: &pango::Layout,
    m: &Metrics,
    palette: &Palette,
    cs: &CellState,
    col: usize,
    row: usize,
) {
    let attrs = cs.attrs;
    // The continuation half of a wide glyph and non-printing cells draw nothing
    // (their background is already filled).
    if attrs.contains(CellAttrs::WIDE_CHAR_SPACER)
        || attrs.contains(CellAttrs::HIDDEN)
        || cs.c == ' '
        || cs.c == '\0'
        || cs.c.is_control()
    {
        return;
    }

    let x = col as f64 * m.cell_w;
    let y = row as f64 * m.cell_h;

    let (fr, fg, fb) = if attrs.contains(CellAttrs::DEFAULT_FG) {
        (palette.fg.r, palette.fg.g, palette.fg.b)
    } else {
        (cs.fg.r, cs.fg.g, cs.fg.b)
    };
    // Pango has no "dim" attribute; emulate with reduced foreground alpha.
    let alpha = if attrs.contains(CellAttrs::DIM) {
        0.55
    } else {
        1.0
    };
    cr.set_source_rgba(
        fr as f64 / 255.0,
        fg as f64 / 255.0,
        fb as f64 / 255.0,
        alpha,
    );

    if attrs.contains(CellAttrs::BOLD) || attrs.contains(CellAttrs::ITALIC) {
        let mut font = m.font.clone();
        if attrs.contains(CellAttrs::BOLD) {
            font.set_weight(pango::Weight::Bold);
        }
        if attrs.contains(CellAttrs::ITALIC) {
            font.set_style(pango::Style::Italic);
        }
        layout.set_font_description(Some(&font));
    } else {
        layout.set_font_description(Some(&m.font));
    }

    let mut buf = [0u8; 4];
    layout.set_text(cs.c.encode_utf8(&mut buf));
    cr.move_to(x, y);
    pangocairo::functions::show_layout(cr, layout);

    // Underline / strikethrough as cairo rules spanning the rendered glyph width
    // (so they cover the full width of a double-width glyph).
    if attrs.contains(CellAttrs::UNDERLINE) || attrs.contains(CellAttrs::STRIKETHROUGH) {
        let gw = (layout.pixel_size().0 as f64).max(m.cell_w);
        cr.set_line_width(1.0);
        if attrs.contains(CellAttrs::UNDERLINE) {
            let uy = (y + m.cell_h - 1.5).round() + 0.5;
            cr.move_to(x, uy);
            cr.line_to(x + gw, uy);
            let _ = cr.stroke();
        }
        if attrs.contains(CellAttrs::STRIKETHROUGH) {
            let sy = (y + m.cell_h * 0.5).round() + 0.5;
            cr.move_to(x, sy);
            cr.line_to(x + gw, sy);
            let _ = cr.stroke();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_cursor(
    cr: &cairo::Context,
    layout: &pango::Layout,
    m: &Metrics,
    palette: &Palette,
    cells: &[CellState],
    cols: usize,
    rows: usize,
    cursor: &kmux_protocol::messages::CursorState,
) {
    let cx = cursor.col as usize;
    let cy = cursor.row as usize;
    if cx >= cols || cy >= rows {
        return;
    }
    let x = cx as f64 * m.cell_w;
    let y = cy as f64 * m.cell_h;
    // Theme-driven cursor color. `cursor_bg` defaults to `fg` (see
    // `kmux_app::theme`), so the default look is unchanged, but a theme can
    // override it independently of the text foreground.
    let cur = palette.cursor_bg;

    match cursor.shape {
        CursorShape::Block => {
            // Inverted block: fill with the cursor color, redraw the glyph in
            // the theme's `cursor_fg` (defaults to `bg`) so it stays legible.
            src(cr, cur.r, cur.g, cur.b);
            cr.rectangle(x, y, m.cell_w, m.cell_h);
            let _ = cr.fill();
            if let Some(cs) = cells.get(cy * cols + cx)
                && cs.c != ' '
                && cs.c != '\0'
                && !cs.c.is_control()
                && !cs.attrs.contains(CellAttrs::WIDE_CHAR_SPACER)
            {
                let fg = palette.cursor_fg;
                src(cr, fg.r, fg.g, fg.b);
                layout.set_font_description(Some(&m.font));
                let mut buf = [0u8; 4];
                layout.set_text(cs.c.encode_utf8(&mut buf));
                cr.move_to(x, y);
                pangocairo::functions::show_layout(cr, layout);
            }
        }
        CursorShape::HollowBlock => {
            src(cr, cur.r, cur.g, cur.b);
            cr.set_line_width(1.0);
            cr.rectangle(x + 0.5, y + 0.5, m.cell_w - 1.0, m.cell_h - 1.0);
            let _ = cr.stroke();
        }
        CursorShape::Underline => {
            src(cr, cur.r, cur.g, cur.b);
            cr.rectangle(x, y + m.cell_h - 2.0, m.cell_w, 2.0);
            let _ = cr.fill();
        }
        CursorShape::Bar => {
            src(cr, cur.r, cur.g, cur.b);
            cr.rectangle(x, y, 2.0, m.cell_h);
            let _ = cr.fill();
        }
        CursorShape::Hidden => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_scroll_indicator(
    cr: &cairo::Context,
    layout: &pango::Layout,
    m: &Metrics,
    palette: &Palette,
    offset: usize,
    total: usize,
    width: i32,
) {
    let label = format!("[{offset}/{total}]");
    layout.set_font_description(Some(&m.font));
    layout.set_text(&label);
    let (lw, lh) = layout.pixel_size();
    let x = (width as f64 - lw as f64 - m.cell_w).max(0.0);
    let h = m.cell_h.max(lh as f64);
    cr.set_source_rgb(0.0, 0.0, 0.0);
    cr.rectangle(x, 0.0, lw as f64, h);
    let _ = cr.fill();
    src(cr, palette.yellow.r, palette.yellow.g, palette.yellow.b);
    cr.move_to(x, 0.0);
    pangocairo::functions::show_layout(cr, layout);
}

fn placeholder(
    cr: &cairo::Context,
    ctx: &pango::Context,
    m: &Metrics,
    palette: &Palette,
    width: i32,
    height: i32,
) {
    let layout = pango::Layout::new(ctx);
    layout.set_font_description(Some(&m.font));
    layout.set_text("No active session — press Ctrl+G then s, c to create one");
    let (lw, lh) = layout.pixel_size();
    let x = ((width - lw).max(0) / 2) as f64;
    let y = ((height - lh).max(0) / 2) as f64;
    src(cr, palette.fg_dim.r, palette.fg_dim.g, palette.fg_dim.b);
    cr.move_to(x, y);
    pangocairo::functions::show_layout(cr, &layout);
}

/// Parse a Pango font-description string (e.g. `"monospace 11"`), falling back
/// to a monospace default if it is empty/unparseable so we never render with a
/// proportional fallback that breaks the grid.
pub fn font_from_str(s: &str) -> pango::FontDescription {
    let desc = pango::FontDescription::from_string(s);
    if desc.family().is_none() {
        desc_with_monospace(desc)
    } else {
        desc
    }
}

/// Ensure a font description has a family, defaulting to `monospace`.
fn desc_with_monospace(mut desc: pango::FontDescription) -> pango::FontDescription {
    desc.set_family("monospace");
    if desc.size() == 0 {
        desc.set_size(11 * pango::SCALE);
    }
    desc
}
