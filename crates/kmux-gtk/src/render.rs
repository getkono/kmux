//! Cairo + Pango rendering of `AppCore`'s active terminal grid.
//!
//! The cell geometry is derived from the configured font ([`Metrics`]) rather
//! than hardcoded, so the grid stays aligned at any font size or display scale.
//! This is the GTK analog of the TUI's `ui/grid.rs`.

use gtk4::cairo;
use gtk4::pango;

use kmux_app::core::AppCore;

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

/// Set the cairo source to an 8-bit RGB triple.
fn src(cr: &cairo::Context, r: u8, g: u8, b: u8) {
    cr.set_source_rgb(r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
}

/// Paint the active grid into `cr`. Basic per-cell Pango glyph drawing —
/// attributes, wide chars, cursor shapes, and scrollback land in a follow-up.
pub fn render(core: &AppCore, cr: &cairo::Context, ctx: &pango::Context, metrics: &Metrics) {
    let bg = core.palette.bg;
    src(cr, bg.r, bg.g, bg.b);
    let _ = cr.paint();

    let layout = pango::Layout::new(ctx);
    layout.set_font_description(Some(&metrics.font));

    let Some(grid) = core.mgr.active_grid() else {
        let fg = core.palette.fg;
        src(cr, fg.r, fg.g, fg.b);
        layout.set_text("kmux — connecting to local daemon…");
        cr.move_to(metrics.cell_w, metrics.cell_h);
        pangocairo::functions::show_layout(cr, &layout);
        return;
    };

    let cells = grid.cells();
    let cols = grid.cols;
    let mut buf = [0u8; 4];
    for row in 0..grid.rows {
        for col in 0..cols {
            let Some(cell) = cells.get(row * cols + col) else {
                continue;
            };
            let x = col as f64 * metrics.cell_w;
            let y = row as f64 * metrics.cell_h;

            src(cr, cell.bg.r, cell.bg.g, cell.bg.b);
            cr.rectangle(x, y, metrics.cell_w, metrics.cell_h);
            let _ = cr.fill();

            if cell.c != ' ' && cell.c != '\0' && !cell.c.is_control() {
                src(cr, cell.fg.r, cell.fg.g, cell.fg.b);
                layout.set_text(cell.c.encode_utf8(&mut buf));
                cr.move_to(x, y);
                pangocairo::functions::show_layout(cr, &layout);
            }
        }
    }

    // Basic block cursor (full cursor-shape fidelity in a follow-up).
    let cur = grid.cursor();
    if cur.visible {
        let fg = core.palette.fg;
        cr.set_source_rgba(
            fg.r as f64 / 255.0,
            fg.g as f64 / 255.0,
            fg.b as f64 / 255.0,
            0.6,
        );
        cr.rectangle(
            cur.col as f64 * metrics.cell_w,
            cur.row as f64 * metrics.cell_h,
            metrics.cell_w,
            metrics.cell_h,
        );
        let _ = cr.fill();
    }
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
