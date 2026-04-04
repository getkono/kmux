use ratatui::style::Color;

// One Dark palette
pub const BG: Color = Color::Rgb(0x28, 0x2c, 0x34);
pub const FG: Color = Color::Rgb(0xab, 0xb2, 0xbf);
pub const FG_DIM: Color = Color::Rgb(0x5c, 0x63, 0x70);
pub const ACCENT: Color = Color::Rgb(0x61, 0xaf, 0xef);
pub const GREEN: Color = Color::Rgb(0x98, 0xc3, 0x79);
pub const RED: Color = Color::Rgb(0xe0, 0x6c, 0x75);
pub const YELLOW: Color = Color::Rgb(0xe5, 0xc0, 0x7b);
pub const PURPLE: Color = Color::Rgb(0xc6, 0x78, 0xdd);
pub const ORANGE: Color = Color::Rgb(0xd1, 0x9a, 0x66);
pub const STATUS_BG: Color = Color::Rgb(0x21, 0x25, 0x2b);

/// Map a protocol CellColor to a ratatui Color.
pub fn cell_color(c: kmux_protocol::messages::CellColor) -> Color {
    Color::Rgb(c.r, c.g, c.b)
}
