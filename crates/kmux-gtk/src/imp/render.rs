//! Cairo + Pango rendering of `AppCore`'s active terminal grid.
//!
//! The cell geometry is derived from the configured font ([`Metrics`]) rather
//! than hardcoded, so the grid stays aligned at any font size or display scale.
//! It renders the `CellGrid` into Cairo/Pango, handling
//! default colors, wide-char spacers, control chars, text attributes, cursor
//! shapes, and scrollback.

use gtk4::cairo;
use gtk4::pango;
use gtk4::pango::IsAttribute;

use kmux_app::appearance::Appearance;
use kmux_app::core::AppCore;
use kmux_app::layout::{LayoutConfig, resolve_layout};
use kmux_app::theme::Theme as Palette;
use kmux_client::grid::{CellGrid, ScrollbackBuffer, scrollback_display_row_at};
use kmux_protocol::messages::{CellAttrs, CellState, CursorShape, PaneInfo};

/// Divider thickness (cells) between tiled panes — must match the value the
/// client uses when computing per-pane sizes (`tiles::push_sizes`).
pub const GUTTER: u16 = 1;

/// Cell geometry + fonts derived from the configured [`Appearance`]. Recomputed
/// when the appearance or the display scale factor changes.
pub struct Metrics {
    /// Cell advance width in (logical) pixels (after `adjust-cell-width`).
    pub cell_w: f64,
    /// Cell height (ascent + descent) in (logical) pixels (after `adjust-cell-height`).
    pub cell_h: f64,
    /// The regular font, reused to render glyphs and shown in the prefs entry.
    pub font: pango::FontDescription,
    /// Bold face: an explicit `font-family-bold`, else synthetic bold of `font`.
    font_bold: pango::FontDescription,
    /// Italic face: an explicit `font-family-italic`, else synthetic italic.
    font_italic: pango::FontDescription,
    /// Bold-italic face: an explicit family, else synthetic bold+italic.
    font_bold_italic: pango::FontDescription,
    /// OpenType feature attributes applied to every glyph, or `None` when no
    /// `font-feature`s are configured.
    features: Option<pango::AttrList>,
}

impl Metrics {
    /// Measure cell size + build the per-style fonts for `appearance` using
    /// `ctx` — a widget's `PangoContext`, which carries the display font map,
    /// resolution, and scale factor, so the result is in the same (logical)
    /// pixel space the `DrawingArea` draws in.
    pub fn measure(ctx: &pango::Context, appearance: &Appearance) -> Self {
        let font = font_desc_from_appearance(appearance);
        let fm = ctx.metrics(Some(&font), None);
        let line_h = (fm.ascent() + fm.descent()) as f64 / pango::SCALE as f64;
        let cell_h = appearance.cell_height_adjust.apply(line_h).ceil().max(1.0);

        // Measure a representative advance for the (monospace) face; ceil so
        // cells tile without sub-pixel seams.
        let layout = pango::Layout::new(ctx);
        layout.set_font_description(Some(&font));
        layout.set_text("M");
        let char_w = layout.size().0 as f64 / pango::SCALE as f64;
        let cell_w = appearance.cell_width_adjust.apply(char_w).ceil().max(1.0);

        // OpenType feature attributes (per-glyph; cross-cell ligatures don't
        // apply to a cell-by-cell renderer — see `kmux_app::appearance`).
        let features = appearance.feature_string().map(|s| {
            let list = pango::AttrList::new();
            list.insert(pango::AttrFontFeatures::new(&s).upcast());
            list
        });

        Self {
            cell_w,
            cell_h,
            font_bold: variant_desc(appearance, appearance.family_bold.as_deref(), true, false),
            font_italic: variant_desc(appearance, appearance.family_italic.as_deref(), false, true),
            font_bold_italic: variant_desc(
                appearance,
                appearance.family_bold_italic.as_deref(),
                true,
                true,
            ),
            font,
            features,
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

/// Lay out the active tab's panes and paint each into its sub-rectangle, with a
/// focus border around the focused pane when more than one is visible. Falls
/// back to a single full-area grid (or the placeholder) when there's no tab.
///
/// `width`/`height` are the content area's logical pixel size.
pub fn render_tiled(
    core: &AppCore,
    cr: &cairo::Context,
    ctx: &pango::Context,
    metrics: &Metrics,
    width: i32,
    height: i32,
    cursor_phase: bool,
) {
    let palette = &core.palette;
    // Window background shows in the gutters between tiles.
    src(cr, palette.bg.r, palette.bg.g, palette.bg.b);
    let _ = cr.paint();

    let Some(layout) = core.mgr.render_layout() else {
        // No tab: single active grid, or the no-session placeholder.
        match core.mgr.active_grid() {
            Some(grid) => paint_grid(grid, cr, ctx, metrics, palette, width, height, cursor_phase),
            None => placeholder(cr, ctx, metrics, palette, width, height),
        }
        return;
    };

    let (cols, rows) = metrics.cols_rows(width, height);
    let cfg = LayoutConfig {
        gutter_cols: GUTTER,
        gutter_rows: GUTTER,
        min_cols: 1,
        min_rows: 1,
    };
    let rects = resolve_layout(&layout, cols, rows, &cfg);
    let multi = rects.len() > 1;
    let focused = core
        .mgr
        .active_pane_id()
        .and_then(|p| p.rsplit_once('/'))
        .and_then(|(_, i)| i.parse::<u32>().ok());
    let word = core.mgr.active_session().unwrap_or("").to_string();

    for r in &rects {
        let px = r.col as f64 * metrics.cell_w;
        let py = r.row as f64 * metrics.cell_h;
        let pw = r.cols as f64 * metrics.cell_w;
        let ph = r.rows as f64 * metrics.cell_h;
        let pane_id = format!("{word}/{}", r.pane_index);
        if let Some(grid) = core.mgr.buffer(&pane_id) {
            let _ = cr.save();
            cr.rectangle(px, py, pw, ph);
            cr.clip();
            cr.translate(px, py);
            paint_grid(
                grid,
                cr,
                ctx,
                metrics,
                palette,
                pw as i32,
                ph as i32,
                cursor_phase,
            );
            let _ = cr.restore();
        }
        if multi {
            // Accent border on the focused pane; a dim divider on the others.
            let (c, lw) = if Some(r.pane_index) == focused {
                (palette.accent, 2.0)
            } else {
                (palette.status_bg, 1.0)
            };
            src(cr, c.r, c.g, c.b);
            cr.set_line_width(lw);
            cr.rectangle(
                px + lw / 2.0,
                py + lw / 2.0,
                (pw - lw).max(0.0),
                (ph - lw).max(0.0),
            );
            let _ = cr.stroke();
        }

        // OSC 9;4 progress (issue #125): a thin bar along the pane's bottom edge,
        // colored by state, width proportional to the percentage (full-width for
        // the indeterminate state). Painted last so it sits over the grid/border.
        if let Some(info) = core.mgr.pane_info(&pane_id)
            && let Some(((cr8, cg8, cb8), frac)) = progress_bar_fill(info, palette)
        {
            let bar_h = 3.0_f64.min(ph);
            let bar_w = (pw * frac).clamp(0.0, pw);
            if bar_w > 0.0 {
                src(cr, cr8, cg8, cb8);
                cr.rectangle(px, py + ph - bar_h, bar_w, bar_h);
                let _ = cr.fill();
            }
        }
    }
}

/// The `(color, width-fraction)` for a pane's OSC 9;4 progress bar, or `None`
/// when no bar should be drawn (`Remove`). `Indeterminate` fills the full width;
/// the numeric states use `progress`/100. Colours: set→accent, error→red,
/// pause→orange.
fn progress_bar_fill(info: &PaneInfo, palette: &Palette) -> Option<((u8, u8, u8), f64)> {
    use kmux_protocol::messages::PaneProgressState as S;
    let frac = f64::from(info.progress.unwrap_or(0).min(100)) / 100.0;
    let (c, fraction) = match info.progress_state {
        S::Remove => return None,
        S::Set => (palette.accent, frac),
        S::Error => (palette.red, frac),
        S::Pause => (palette.orange, frac),
        S::Indeterminate => (palette.accent, 1.0),
    };
    Some(((c.r, c.g, c.b), fraction))
}

/// Paint a single `grid` filling the current cairo target (origin at `(0,0)`).
/// `width`/`height` are the target's logical pixel size.
#[allow(clippy::too_many_arguments)]
fn paint_grid(
    grid: &CellGrid,
    cr: &cairo::Context,
    ctx: &pango::Context,
    metrics: &Metrics,
    palette: &Palette,
    width: i32,
    _height: i32,
    cursor_phase: bool,
) {
    src(cr, palette.bg.r, palette.bg.g, palette.bg.b);
    let _ = cr.paint();

    let layout = pango::Layout::new(ctx);
    // OpenType features apply to every glyph drawn with this layout (the
    // attribute spans the whole text, which we reset per cell).
    layout.set_attributes(metrics.features.as_ref());
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

    // Pick the matching face (explicit variant family, else synthetic style).
    let desc = match (
        attrs.contains(CellAttrs::BOLD),
        attrs.contains(CellAttrs::ITALIC),
    ) {
        (true, true) => &m.font_bold_italic,
        (true, false) => &m.font_bold,
        (false, true) => &m.font_italic,
        (false, false) => &m.font,
    };
    layout.set_font_description(Some(desc));

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

    // Render-debug trace: the Cairo path's hardcoded cursor geometry (2px
    // bar/underline below) side-by-side with what kmux-render's `cursor_geometry`
    // would compute (scale-aware `cursor_thickness`). The divergence is the prime
    // suspect for incorrect cursor rendering — enable with
    // `RUST_LOG="kmux::render_debug=trace"`.
    if tracing::enabled!(target: "kmux::render_debug", tracing::Level::TRACE) {
        let rcell = kmux_render::CellMetrics::new(m.cell_w as f32, m.cell_h as f32);
        let cv = kmux_render::CursorView {
            col: cursor.col,
            row: cursor.row,
            shape: cursor.shape,
            blink: cursor.blink,
            visible: cursor.visible,
        };
        let geo = kmux_render::cursor_geometry(&cv, (0.0, 0.0), cols as u16, rows as u16, &rcell);
        tracing::trace!(
            target: "kmux::render_debug",
            col = cursor.col,
            row = cursor.row,
            shape = ?cursor.shape,
            cairo_x = x,
            cairo_y = y,
            cairo_bar_underline_thickness = 2.0_f64,
            renderer_cursor_thickness = rcell.cursor_thickness,
            renderer_rect0 = ?geo.rects.first(),
            "cairo cursor vs kmux-render geometry"
        );
    }

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

/// Build the regular [`pango::FontDescription`] for an [`Appearance`] — family
/// (+ optional named style) and size, via Pango's font-description grammar.
pub fn font_desc_from_appearance(a: &Appearance) -> pango::FontDescription {
    let spec = match a.style.as_deref() {
        Some(style) if !style.is_empty() => format!("{} {} {}", a.family, style, a.size_pt),
        _ => format!("{} {}", a.family, a.size_pt),
    };
    ensure_monospace(pango::FontDescription::from_string(&spec), a.size_pt)
}

/// Build the bold/italic/bold-italic face: an explicit variant `family` when
/// configured, otherwise the base face with synthetic weight/style applied.
fn variant_desc(
    a: &Appearance,
    family: Option<&str>,
    bold: bool,
    italic: bool,
) -> pango::FontDescription {
    match family {
        Some(fam) if !fam.is_empty() => ensure_monospace(
            pango::FontDescription::from_string(&format!("{fam} {}", a.size_pt)),
            a.size_pt,
        ),
        _ => {
            let mut desc = font_desc_from_appearance(a);
            if bold {
                desc.set_weight(pango::Weight::Bold);
            }
            if italic {
                desc.set_style(pango::Style::Italic);
            }
            desc
        }
    }
}

/// Ensure a font description has a family + size, defaulting to `monospace` at
/// `size_pt` so we never render with a proportional fallback that breaks the grid.
fn ensure_monospace(mut desc: pango::FontDescription, size_pt: f32) -> pango::FontDescription {
    if desc.family().is_none() {
        desc.set_family("monospace");
    }
    if desc.size() == 0 {
        desc.set_size((size_pt * pango::SCALE as f32) as i32);
    }
    desc
}
