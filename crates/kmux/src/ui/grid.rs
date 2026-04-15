use kmux_protocol::messages::{CellAttrs, CursorShape};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use crate::app::App;
use crate::theme;

pub(super) fn render_grid(f: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    // Track cursor position to set after buffer operations
    let mut cursor_pos: Option<(u16, u16)> = None;

    {
        let buf = f.buffer_mut();

        // Fill background
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                let cell = &mut buf[(x, y)];
                cell.set_char(' ');
                cell.set_bg(theme.bg);
                cell.set_fg(theme.fg);
            }
        }

        let Some(name) = app.mgr.active_pane_id() else {
            // No active pane message
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

        let cells = grid.cells();
        let rows = grid.rows;
        let cols = grid.cols;
        let scroll_offset = grid.scroll_offset();
        let scrollback = grid.scrollback();

        // Render cells
        for vr in 0..rows.min(area.height as usize) {
            for vc in 0..cols.min(area.width as usize) {
                let screen_x = area.left() + vc as u16;
                let screen_y = area.top() + vr as u16;
                if screen_x >= area.right() || screen_y >= area.bottom() {
                    continue;
                }

                let cell_state = if scroll_offset > 0 && vr < scroll_offset {
                    let sb_len = scrollback.len();
                    let sb_idx = sb_len.saturating_sub(scroll_offset) + vr;
                    scrollback.get(sb_idx).and_then(|line| line.get(vc))
                } else {
                    let grid_row = if scroll_offset > 0 {
                        vr - scroll_offset
                    } else {
                        vr
                    };
                    cells.get(grid_row * cols + vc)
                };

                if let Some(cs) = cell_state {
                    let ratatui_cell = &mut buf[(screen_x, screen_y)];

                    if cs.attrs.contains(CellAttrs::WIDE_CHAR_SPACER) {
                        continue;
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
                    ratatui_cell.set_style(Style::default().add_modifier(modifier));
                }
            }
        }

        // Render cursor
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

        // Scroll indicator
        if scroll_offset > 0 {
            let label = format!("[{}/{}]", scroll_offset, scrollback.len());
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
