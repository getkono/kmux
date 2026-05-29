use kmux_protocol::messages::{CellAttrs, CellState, CursorShape};
use ratatui::Frame;
use ratatui::buffer::Cell;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use crate::app::App;
use crate::theme::{self, Theme};

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

pub(super) fn render_grid(f: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let mut cursor_pos: Option<(u16, u16)> = None;

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
        let cursor = grid.cursor();
        if scroll_offset == 0 && cursor.visible && cursor.shape != CursorShape::Hidden {
            let cur_row = cursor.row as usize;
            let cur_col = cursor.col as usize;
            if cur_row < area.height as usize && cur_col < area.width as usize {
                let cx = area.left() + cur_col as u16;
                let cy = area.top() + cur_row as u16;
                if cx < area.right() && cy < area.bottom() {
                    match cursor.shape {
                        CursorShape::Block => {
                            let cell = &mut buf[(cx, cy)];
                            let fg = cell.bg;
                            let bg = Color::White;
                            cell.set_fg(fg);
                            cell.set_bg(bg);
                        }
                        CursorShape::Underline | CursorShape::Bar => {
                            cursor_pos = Some((cx, cy));
                        }
                        CursorShape::HollowBlock => {
                            let cell = &mut buf[(cx, cy)];
                            cell.set_style(
                                Style::default()
                                    .fg(Color::White)
                                    .add_modifier(Modifier::SLOW_BLINK),
                            );
                        }
                        CursorShape::Hidden => {}
                    }
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

    if let Some((cx, cy)) = cursor_pos {
        f.set_cursor_position((cx, cy));
    }
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
}
