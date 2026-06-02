use kmux_protocol::messages::{CellAttrs, CellState, CursorShape};
use ratatui::Frame;
use ratatui::buffer::Cell;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use crate::app::App;
use crate::theme::{self, Theme};

/// Glyph drawn in the cursor cell when the inner pane requests a Bar shape
/// (DECSCUSR `\x1b[5 q`). U+258F LEFT ONE EIGHTH BLOCK gives a thin vertical
/// bar at the leading edge of the cell that matches what most native terminals
/// draw for a bar cursor.
const BAR_CURSOR_GLYPH: &str = "\u{258f}";

/// Glyph drawn in the cursor cell when the inner pane requests an Underline
/// shape (DECSCUSR `\x1b[3 q`). U+2581 LOWER ONE EIGHTH BLOCK draws a thin
/// horizontal bar across the bottom of the cell.
const UNDERLINE_CURSOR_GLYPH: &str = "\u{2581}";

/// Apply a single protocol cell to a ratatui buffer cell.
///
/// The cell must already be in its background-fill state (reset + theme
/// colors).  Control characters are skipped because printing them via
/// crossterm desynchronises the backend's cursor tracker from the real
/// terminal cursor, smearing the rest of the row.
fn apply_cell(ratatui_cell: &mut Cell, cs: &CellState, theme: &Theme) {
    if cs.attrs.contains(CellAttrs::WIDE_CHAR_SPACER) {
        // Continuation half of a double-width glyph.  Keep the background-fill
        // symbol (" ") — an empty symbol would not advance the cursor if this
        // cell ever appears in the diff output.  Only patch the bg colour.
        let spacer_bg = if cs.attrs.contains(CellAttrs::DEFAULT_BG) {
            theme.bg
        } else {
            theme::cell_color(cs.bg)
        };
        ratatui_cell.set_bg(spacer_bg);
        return;
    }

    // Control characters have no visible width but crossterm still prints
    // them, causing the real cursor to lag behind the tracked position.
    // Leave the cell in its background-fill state (a normal space).
    if cs.c.is_control() {
        return;
    }

    ratatui_cell.set_char(cs.c);

    let display_fg = if cs.attrs.contains(CellAttrs::DEFAULT_FG) {
        theme.fg
    } else {
        theme::cell_color(cs.fg)
    };
    let display_bg = if cs.attrs.contains(CellAttrs::DEFAULT_BG) {
        theme.bg
    } else {
        theme::cell_color(cs.bg)
    };
    ratatui_cell.set_fg(display_fg);
    ratatui_cell.set_bg(display_bg);

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
    if !modifier.is_empty() {
        ratatui_cell.set_style(Style::default().add_modifier(modifier));
    }
}

/// Paint the inner-pane cursor into a single ratatui cell.
///
/// Bar and Underline are drawn as cell glyphs (`▏`, `▁`) instead of being
/// delegated to the host terminal's hardware cursor — the host cursor is often
/// hidden in alternate-screen mode or has the wrong shape, leaving users with
/// an invisible cursor in apps like Claude Code that request a bar shape via
/// DECSCUSR. Drawing in-cell is reliable across every host.
///
/// `blink` carries the DECSCUSR `blinking_*` request: when set we add
/// `Modifier::SLOW_BLINK` so the host terminal blinks the cursor cell. This
/// mirrors the GTK frontend honoring the blink request rather than blinking
/// every cursor, so a steady cursor (`steady_*`) stays solid.
fn paint_cursor_cell(cell: &mut Cell, shape: CursorShape, blink: bool, theme: &Theme) {
    match shape {
        CursorShape::Block => {
            cell.set_bg(theme.cursor_bg);
            cell.set_fg(theme.cursor_fg);
        }
        CursorShape::Bar => {
            cell.set_symbol(BAR_CURSOR_GLYPH);
            cell.set_fg(theme.cursor_bg);
        }
        CursorShape::Underline => {
            cell.set_symbol(UNDERLINE_CURSOR_GLYPH);
            cell.set_fg(theme.cursor_bg);
        }
        CursorShape::HollowBlock => {
            cell.set_style(
                Style::default()
                    .fg(theme.cursor_bg)
                    .add_modifier(Modifier::SLOW_BLINK),
            );
        }
        CursorShape::Hidden => return,
    }
    if blink {
        cell.modifier.insert(Modifier::SLOW_BLINK);
    }
}

pub(super) fn render_grid(f: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;

    {
        let buf = f.buffer_mut();

        // Fill background — reset() clears symbol, modifiers, skip, and
        // underline_color so no stale state from a prior frame leaks through.
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                let cell = &mut buf[(x, y)];
                cell.reset();
                cell.set_bg(theme.bg);
                cell.set_fg(theme.fg);
            }
        }

        let Some(name) = app.mgr.active_pane_id() else {
            let msg = "No active session -- press Ctrl+G then s, c to create one";
            let x = area.left() + area.width.saturating_sub(msg.len() as u16) / 2;
            let y = area.top() + area.height / 2;
            if y < area.bottom() {
                for (i, ch) in msg.chars().enumerate() {
                    let px = x + i as u16;
                    if px < area.right() {
                        let cell = &mut buf[(px, y)];
                        cell.set_char(ch);
                        cell.set_fg(theme.fg_dim);
                    }
                }
            }
            return;
        };

        let Some(grid) = app.mgr.buffer(name) else {
            return;
        };

        // ── Cells ──
        let cells = grid.cells();
        let rows = grid.rows;
        let cols = grid.cols;
        let scroll_offset = grid.scroll_offset();
        let scrollback = grid.scrollback();

        for vr in 0..rows.min(area.height as usize) {
            let sb_row = if scroll_offset > 0 && vr < scroll_offset {
                let rev = scroll_offset - 1 - vr;
                kmux_client::grid::scrollback_display_row_at(scrollback, cols, rev)
            } else {
                None
            };

            for vc in 0..cols.min(area.width as usize) {
                let screen_x = area.left() + vc as u16;
                let screen_y = area.top() + vr as u16;
                if screen_x >= area.right() || screen_y >= area.bottom() {
                    continue;
                }

                let cell_state = if let Some((line_idx, col_start)) = sb_row {
                    scrollback
                        .get(line_idx)
                        .and_then(|line| line.get(col_start + vc))
                } else if scroll_offset > 0 {
                    let grid_row = vr - scroll_offset;
                    cells.get(grid_row * cols + vc)
                } else {
                    cells.get(vr * cols + vc)
                };

                if let Some(cs) = cell_state {
                    apply_cell(&mut buf[(screen_x, screen_y)], cs, theme);
                }
            }
        }

        // ── Cursor ──
        //
        // Every cursor shape is drawn directly into the cell buffer. We do
        // *not* delegate to ratatui's `Frame::set_cursor_position` for
        // Bar/Underline because that just shows the host terminal's hardware
        // cursor — which has the user's default shape (often a block), can be
        // hidden by some terminals in alternate-screen mode, and can sit
        // underneath the painted cell content invisibly. Drawing the cursor
        // in-cell makes it visible everywhere regardless of host behaviour.
        let cursor = grid.cursor();
        if scroll_offset == 0 && cursor.visible && cursor.shape != CursorShape::Hidden {
            let cur_row = cursor.row as usize;
            let cur_col = cursor.col as usize;
            if cur_row < area.height as usize && cur_col < area.width as usize {
                let cx = area.left() + cur_col as u16;
                let cy = area.top() + cur_row as u16;
                if cx < area.right() && cy < area.bottom() {
                    paint_cursor_cell(&mut buf[(cx, cy)], cursor.shape, cursor.blink, theme);
                }
            }
        }

        // ── Scroll indicator ──
        if scroll_offset > 0 {
            let label = format!(
                "[{}/{}]",
                scroll_offset,
                grid.total_scrollback_display_rows()
            );
            let x = area.right().saturating_sub(label.len() as u16 + 1);
            let y = area.top();
            if y < area.bottom() {
                for (i, ch) in label.chars().enumerate() {
                    let px = x + i as u16;
                    if px < area.right() {
                        let cell = &mut buf[(px, y)];
                        cell.set_char(ch);
                        cell.set_fg(theme.yellow);
                        cell.set_bg(Color::Rgb(0, 0, 0));
                    }
                }
            }
        }
    } // buf borrow ends here
}

#[cfg(test)]
mod tests {
    use super::*;
    use kmux_protocol::messages::{CellAttrs, CellColor};

    fn wide_spacer(bg: CellColor) -> CellState {
        CellState {
            c: ' ',
            fg: CellColor::new(0, 0, 0),
            bg,
            attrs: CellAttrs(CellAttrs::WIDE_CHAR_SPACER),
        }
    }

    #[test]
    fn wide_char_spacer_preserves_space_symbol() {
        let theme = theme::default_theme();
        let mut cell = Cell::default();
        cell.set_char(' ');

        apply_cell(&mut cell, &wide_spacer(CellColor::new(0, 0, 0)), &theme);

        assert_eq!(
            cell.symbol(),
            " ",
            "spacer cell must keep ' ' so crossterm advances the cursor",
        );
    }

    #[test]
    fn wide_char_spacer_default_bg_uses_theme_bg() {
        let theme = theme::default_theme();
        let mut cell = Cell::default();
        cell.set_char(' ');
        let mut cs = wide_spacer(CellColor::new(12, 34, 56));
        cs.attrs = CellAttrs(CellAttrs::WIDE_CHAR_SPACER | CellAttrs::DEFAULT_BG);

        apply_cell(&mut cell, &cs, &theme);

        assert_eq!(cell.symbol(), " ");
        assert_eq!(cell.bg, theme.bg);
    }

    #[test]
    fn non_spacer_sets_char_and_colors() {
        let theme = theme::default_theme();
        let mut cell = Cell::default();
        let cs = CellState {
            c: 'X',
            fg: CellColor::new(255, 0, 0),
            bg: CellColor::new(0, 255, 0),
            attrs: CellAttrs::EMPTY,
        };

        apply_cell(&mut cell, &cs, &theme);

        assert_eq!(cell.symbol(), "X");
        assert_eq!(cell.fg, Color::Rgb(255, 0, 0));
        assert_eq!(cell.bg, Color::Rgb(0, 255, 0));
    }

    #[test]
    fn control_char_leaves_cell_unchanged() {
        let theme = theme::default_theme();
        let mut cell = Cell::default();
        cell.set_char(' ');
        cell.set_fg(theme.fg);
        cell.set_bg(theme.bg);
        let cs = CellState {
            c: '\0',
            fg: CellColor::new(255, 0, 0),
            bg: CellColor::new(0, 255, 0),
            attrs: CellAttrs::EMPTY,
        };

        apply_cell(&mut cell, &cs, &theme);

        assert_eq!(cell.symbol(), " ", "control char must not be written");
        assert_eq!(cell.fg, theme.fg, "fg must stay at background-fill value");
        assert_eq!(cell.bg, theme.bg, "bg must stay at background-fill value");
    }

    #[test]
    fn modifiers_applied() {
        let theme = theme::default_theme();
        let mut cell = Cell::default();
        let cs = CellState {
            c: 'B',
            fg: CellColor::new(0, 0, 0),
            bg: CellColor::new(0, 0, 0),
            attrs: CellAttrs(CellAttrs::BOLD | CellAttrs::ITALIC),
        };

        apply_cell(&mut cell, &cs, &theme);

        assert!(cell.modifier.contains(Modifier::BOLD));
        assert!(cell.modifier.contains(Modifier::ITALIC));
    }

    fn fill_cell(cell: &mut Cell, ch: char, theme: &Theme) {
        cell.set_char(ch);
        cell.set_fg(theme.fg);
        cell.set_bg(theme.bg);
    }

    #[test]
    fn block_cursor_uses_theme_cursor_colors() {
        // Regression: the previous implementation hardcoded `Color::White` for
        // the bg, which could be invisible on light themes. After the fix the
        // cursor must use `theme.cursor_bg` / `theme.cursor_fg` so contrast is
        // theme-driven.
        let theme = theme::default_theme();
        let mut cell = Cell::default();
        fill_cell(&mut cell, 'a', &theme);

        paint_cursor_cell(&mut cell, CursorShape::Block, false, &theme);

        assert_eq!(cell.bg, theme.cursor_bg);
        assert_eq!(cell.fg, theme.cursor_fg);
        assert_ne!(cell.bg, Color::White, "must not hardcode Color::White");
        // Underlying glyph stays — Block sits on top of it.
        assert_eq!(cell.symbol(), "a");
        // Steady (non-blink) request must not blink.
        assert!(!cell.modifier.contains(Modifier::SLOW_BLINK));
    }

    #[test]
    fn bar_cursor_writes_left_bar_glyph() {
        let theme = theme::default_theme();
        let mut cell = Cell::default();
        fill_cell(&mut cell, 'a', &theme);

        paint_cursor_cell(&mut cell, CursorShape::Bar, false, &theme);

        assert_eq!(cell.symbol(), "\u{258f}", "Bar must paint U+258F");
        assert_eq!(cell.fg, theme.cursor_bg);
    }

    #[test]
    fn underline_cursor_writes_underline_glyph() {
        let theme = theme::default_theme();
        let mut cell = Cell::default();
        fill_cell(&mut cell, 'a', &theme);

        paint_cursor_cell(&mut cell, CursorShape::Underline, false, &theme);

        assert_eq!(cell.symbol(), "\u{2581}", "Underline must paint U+2581");
        assert_eq!(cell.fg, theme.cursor_bg);
    }

    #[test]
    fn hollow_block_cursor_uses_theme_color_and_blinks() {
        let theme = theme::default_theme();
        let mut cell = Cell::default();
        fill_cell(&mut cell, 'a', &theme);

        paint_cursor_cell(&mut cell, CursorShape::HollowBlock, false, &theme);

        assert_eq!(cell.fg, theme.cursor_bg);
        assert!(cell.modifier.contains(Modifier::SLOW_BLINK));
        assert_ne!(cell.fg, Color::White, "must not hardcode Color::White");
    }

    #[test]
    fn hidden_cursor_does_not_modify_cell() {
        let theme = theme::default_theme();
        let mut cell = Cell::default();
        fill_cell(&mut cell, 'a', &theme);
        let before_fg = cell.fg;
        let before_bg = cell.bg;
        let before_sym = cell.symbol().to_string();

        paint_cursor_cell(&mut cell, CursorShape::Hidden, false, &theme);

        assert_eq!(cell.symbol(), before_sym);
        assert_eq!(cell.fg, before_fg);
        assert_eq!(cell.bg, before_bg);
    }

    #[test]
    fn blinking_request_adds_slow_blink_modifier() {
        // A blinking bar (DECSCUSR 5) is painted in-cell and carries
        // SLOW_BLINK so the host terminal blinks it; a steady bar does not.
        let theme = theme::default_theme();

        let mut blinking = Cell::default();
        fill_cell(&mut blinking, 'a', &theme);
        paint_cursor_cell(&mut blinking, CursorShape::Bar, true, &theme);
        assert_eq!(blinking.symbol(), "\u{258f}");
        assert!(blinking.modifier.contains(Modifier::SLOW_BLINK));

        let mut steady = Cell::default();
        fill_cell(&mut steady, 'a', &theme);
        paint_cursor_cell(&mut steady, CursorShape::Bar, false, &theme);
        assert!(!steady.modifier.contains(Modifier::SLOW_BLINK));
    }
}
