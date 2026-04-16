use kmux_protocol::messages::{CellAttrs, CellColor, CellState, CursorShape, CursorState};
use tattoy_wezterm_surface::{CursorShape as WezCursorShape, CursorVisibility};
use tattoy_wezterm_term::{
    Blink, CursorPosition, Intensity, Underline,
    color::{ColorAttribute, ColorPalette, SrgbaTuple},
};

pub(super) fn convert_cursor(pos: &CursorPosition) -> CursorState {
    let visible = pos.visibility == CursorVisibility::Visible;
    CursorState {
        row: pos.y.max(0) as u16,
        col: pos.x as u16,
        shape: if !visible {
            CursorShape::Hidden
        } else {
            convert_cursor_shape(pos.shape)
        },
        visible,
    }
}

pub(super) fn convert_cursor_shape(shape: WezCursorShape) -> CursorShape {
    match shape {
        WezCursorShape::Default | WezCursorShape::BlinkingBlock | WezCursorShape::SteadyBlock => {
            CursorShape::Block
        }
        WezCursorShape::BlinkingUnderline | WezCursorShape::SteadyUnderline => {
            CursorShape::Underline
        }
        WezCursorShape::BlinkingBar | WezCursorShape::SteadyBar => CursorShape::Bar,
    }
}

pub(super) fn cell_state_from_attrs(
    c: char,
    width: usize,
    attrs: &tattoy_wezterm_term::CellAttributes,
    palette: &ColorPalette,
) -> CellState {
    let is_inverse = attrs.reverse();

    // Resolve fg and bg, swapping if INVERSE is set.
    let (fg_attr, bg_attr) = if is_inverse {
        (attrs.background(), attrs.foreground())
    } else {
        (attrs.foreground(), attrs.background())
    };

    let fg = resolve_color(fg_attr, palette);
    let bg = resolve_color(bg_attr, palette);

    // Determine DEFAULT_FG / DEFAULT_BG after accounting for the swap.
    let orig_fg_is_default = attrs.foreground() == ColorAttribute::Default;
    let orig_bg_is_default = attrs.background() == ColorAttribute::Default;
    let (displayed_fg_is_default, displayed_bg_is_default) = if is_inverse {
        (orig_bg_is_default, orig_fg_is_default)
    } else {
        (orig_fg_is_default, orig_bg_is_default)
    };

    let mut bits: u16 = 0;
    // Intensity
    match attrs.intensity() {
        Intensity::Bold => bits |= CellAttrs::BOLD,
        Intensity::Half => bits |= CellAttrs::DIM,
        Intensity::Normal => {}
    }
    if attrs.italic() {
        bits |= CellAttrs::ITALIC;
    }
    if attrs.underline() != Underline::None {
        bits |= CellAttrs::UNDERLINE;
    }
    if attrs.strikethrough() {
        bits |= CellAttrs::STRIKETHROUGH;
    }
    if is_inverse {
        bits |= CellAttrs::INVERSE;
    }
    if attrs.invisible() {
        bits |= CellAttrs::HIDDEN;
    }
    if attrs.blink() != Blink::None {
        bits |= CellAttrs::BLINK;
    }
    if width > 1 {
        bits |= CellAttrs::WIDE_CHAR;
    }
    if displayed_fg_is_default {
        bits |= CellAttrs::DEFAULT_FG;
    }
    if displayed_bg_is_default {
        bits |= CellAttrs::DEFAULT_BG;
    }

    CellState {
        c,
        fg,
        bg,
        attrs: CellAttrs(bits),
    }
}

pub(super) fn resolve_color(attr: ColorAttribute, palette: &ColorPalette) -> CellColor {
    let srgba: SrgbaTuple = palette.resolve_fg(attr);
    let (r, g, b, _) = srgba.as_rgba_u8();
    CellColor::new(r, g, b)
}
