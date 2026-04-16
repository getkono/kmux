use iced::{
    Color as IcedColor, Font, Pixels, Point as IcedPoint, Size, alignment,
    font::{Style, Weight},
    widget::canvas::{self, Text},
};
use kmux_protocol::messages::{CellAttrs, CellState};

use kmux_client::grid::{CELL_HEIGHT, CELL_WIDTH, DEFAULT_BG};

use super::FONT_SIZE;

pub(super) fn cell_color_to_iced(c: kmux_protocol::messages::CellColor) -> IcedColor {
    IcedColor::from_rgb8(c.r, c.g, c.b)
}

pub fn default_bg() -> IcedColor {
    IcedColor::from_rgb8(0x28, 0x2c, 0x34)
}

pub(super) fn draw_cell(frame: &mut canvas::Frame, cell: &CellState, row: usize, col: usize) {
    let x = col as f32 * CELL_WIDTH;
    let y = row as f32 * CELL_HEIGHT;

    // Wide-char spacer: paint background only, skip text/decorations.
    if cell.attrs.contains(CellAttrs::WIDE_CHAR_SPACER) {
        if cell.bg != DEFAULT_BG {
            frame.fill_rectangle(
                IcedPoint::new(x, y),
                Size::new(CELL_WIDTH, CELL_HEIGHT),
                cell_color_to_iced(cell.bg),
            );
        }
        return;
    }

    // Wide chars span two columns.
    let cell_w = if cell.attrs.contains(CellAttrs::WIDE_CHAR) {
        CELL_WIDTH * 2.0
    } else {
        CELL_WIDTH
    };

    if cell.bg != DEFAULT_BG {
        frame.fill_rectangle(
            IcedPoint::new(x, y),
            Size::new(cell_w, CELL_HEIGHT),
            cell_color_to_iced(cell.bg),
        );
    }

    if !cell.attrs.contains(CellAttrs::HIDDEN) && cell.c != ' ' {
        // DIM: blend foreground halfway toward background.
        let fg_color = if cell.attrs.contains(CellAttrs::DIM) {
            let fg = cell_color_to_iced(cell.fg);
            let bg = cell_color_to_iced(cell.bg);
            IcedColor {
                r: (fg.r + bg.r) * 0.5,
                g: (fg.g + bg.g) * 0.5,
                b: (fg.b + bg.b) * 0.5,
                a: 1.0,
            }
        } else {
            cell_color_to_iced(cell.fg)
        };

        // Bold / italic font selection.
        let font = Font {
            family: iced::font::Family::Monospace,
            weight: if cell.attrs.contains(CellAttrs::BOLD) {
                Weight::Bold
            } else {
                Weight::Normal
            },
            style: if cell.attrs.contains(CellAttrs::ITALIC) {
                Style::Italic
            } else {
                Style::Normal
            },
            ..Font::MONOSPACE
        };

        frame.fill_text(Text {
            content: cell.c.to_string(),
            position: IcedPoint::new(x, y),
            color: fg_color,
            size: Pixels(FONT_SIZE),
            line_height: iced::widget::text::LineHeight::Absolute(Pixels(CELL_HEIGHT)),
            font,
            horizontal_alignment: alignment::Horizontal::Left,
            vertical_alignment: alignment::Vertical::Top,
            shaping: iced::widget::text::Shaping::Advanced,
        });

        // Underline decoration.
        if cell.attrs.contains(CellAttrs::UNDERLINE) {
            frame.fill_rectangle(
                IcedPoint::new(x, y + CELL_HEIGHT - 1.0),
                Size::new(cell_w, 1.0),
                fg_color,
            );
        }

        // Strikethrough decoration.
        if cell.attrs.contains(CellAttrs::STRIKETHROUGH) {
            frame.fill_rectangle(
                IcedPoint::new(x, y + CELL_HEIGHT * 0.5),
                Size::new(cell_w, 1.0),
                fg_color,
            );
        }
    }
}
