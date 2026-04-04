use iced::widget::{button, container, text_input};
use iced::{Background, Border, Color, Theme};

// One Dark palette
pub const BG: Color = Color::from_rgb(
    0x28 as f32 / 255.0,
    0x2c as f32 / 255.0,
    0x34 as f32 / 255.0,
);
pub const BG_LIGHTER: Color = Color::from_rgb(
    0x2c as f32 / 255.0,
    0x31 as f32 / 255.0,
    0x3c as f32 / 255.0,
);
pub const BG_LIGHTEST: Color = Color::from_rgb(
    0x3e as f32 / 255.0,
    0x44 as f32 / 255.0,
    0x51 as f32 / 255.0,
);
pub const FG: Color = Color::from_rgb(
    0xab as f32 / 255.0,
    0xb2 as f32 / 255.0,
    0xbf as f32 / 255.0,
);
pub const FG_DIM: Color = Color::from_rgb(
    0x5c as f32 / 255.0,
    0x63 as f32 / 255.0,
    0x70 as f32 / 255.0,
);
pub const ACCENT: Color = Color::from_rgb(
    0x61 as f32 / 255.0,
    0xaf as f32 / 255.0,
    0xef as f32 / 255.0,
);
pub const RED: Color = Color::from_rgb(
    0xe0 as f32 / 255.0,
    0x6c as f32 / 255.0,
    0x75 as f32 / 255.0,
);
pub const GREEN: Color = Color::from_rgb(
    0x98 as f32 / 255.0,
    0xc3 as f32 / 255.0,
    0x79 as f32 / 255.0,
);
pub const YELLOW: Color = Color::from_rgb(
    0xe5 as f32 / 255.0,
    0xc0 as f32 / 255.0,
    0x7b as f32 / 255.0,
);
pub const BORDER: Color = BG_LIGHTEST;

pub fn default() -> Theme {
    Theme::Dark
}

// -- Session bar --

pub fn session_bar_container(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(BG_LIGHTER)),
        border: Border {
            color: BORDER,
            width: 0.0,
            radius: 0.0.into(),
        },
        ..Default::default()
    }
}

pub fn tab_active(_theme: &Theme, _status: button::Status) -> button::Style {
    button::Style {
        background: Some(Background::Color(Color::TRANSPARENT)),
        text_color: Color::WHITE,
        border: Border {
            color: ACCENT,
            width: 2.0,
            radius: 0.0.into(),
        },
        ..Default::default()
    }
}

pub fn tab_inactive(_theme: &Theme, status: button::Status) -> button::Style {
    let text_color = match status {
        button::Status::Hovered => FG,
        _ => FG_DIM,
    };
    button::Style {
        background: Some(Background::Color(Color::TRANSPARENT)),
        text_color,
        border: Border {
            color: Color::TRANSPARENT,
            width: 2.0,
            radius: 0.0.into(),
        },
        ..Default::default()
    }
}

pub fn tab_close(_theme: &Theme, status: button::Status) -> button::Style {
    let text_color = match status {
        button::Status::Hovered => RED,
        _ => FG_DIM,
    };
    button::Style {
        background: Some(Background::Color(Color::TRANSPARENT)),
        text_color,
        border: Border::default(),
        ..Default::default()
    }
}

pub fn tab_new(_theme: &Theme, status: button::Status) -> button::Style {
    let text_color = match status {
        button::Status::Hovered => FG,
        _ => FG_DIM,
    };
    button::Style {
        background: Some(Background::Color(Color::TRANSPARENT)),
        text_color,
        border: Border::default(),
        ..Default::default()
    }
}

// -- Status bar --

pub fn status_bar_container(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(BG_LIGHTER)),
        border: Border {
            color: BORDER,
            width: 1.0,
            radius: 0.0.into(),
        },
        ..Default::default()
    }
}

pub fn leader_active_bar(_theme: &Theme) -> container::Style {
    let accent_tint = Color::from_rgba(ACCENT.r, ACCENT.g, ACCENT.b, 0.15);
    container::Style {
        background: Some(Background::Color(accent_tint)),
        border: Border {
            color: ACCENT,
            width: 0.0,
            radius: 0.0.into(),
        },
        ..Default::default()
    }
}

// -- Connect screen --

pub fn connect_input(_theme: &Theme, status: text_input::Status) -> text_input::Style {
    let border_color = match status {
        text_input::Status::Focused => ACCENT,
        _ => BORDER,
    };
    text_input::Style {
        background: Background::Color(BG),
        border: Border {
            color: border_color,
            width: 1.0,
            radius: 4.0.into(),
        },
        icon: FG_DIM,
        placeholder: FG_DIM,
        value: FG,
        selection: ACCENT,
    }
}

pub fn connect_button(_theme: &Theme, status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Hovered => {
            Color::from_rgb(ACCENT.r * 0.85, ACCENT.g * 0.85, ACCENT.b * 0.85)
        }
        _ => ACCENT,
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: Color::WHITE,
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: 4.0.into(),
        },
        ..Default::default()
    }
}

pub fn connect_container(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(BG_LIGHTER)),
        border: Border {
            color: BORDER,
            width: 1.0,
            radius: 8.0.into(),
        },
        ..Default::default()
    }
}

// -- Overlays --

pub fn overlay_container(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::from_rgba(0.0, 0.0, 0.0, 0.85))),
        ..Default::default()
    }
}

pub fn command_palette_container(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(BG_LIGHTER)),
        border: Border {
            color: BORDER,
            width: 1.0,
            radius: 8.0.into(),
        },
        ..Default::default()
    }
}

pub fn command_palette_item(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::TRANSPARENT)),
        ..Default::default()
    }
}

pub fn command_palette_item_selected(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(ACCENT)),
        ..Default::default()
    }
}

// -- Disconnect button in status bar --

pub fn disconnect_button(_theme: &Theme, status: button::Status) -> button::Style {
    let text_color = match status {
        button::Status::Hovered => RED,
        _ => FG_DIM,
    };
    button::Style {
        background: Some(Background::Color(Color::TRANSPARENT)),
        text_color,
        border: Border::default(),
        ..Default::default()
    }
}

// -- Toast --

pub fn toast_error(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(RED)),
        ..Default::default()
    }
}
