use iced::widget::{Space, button, container, row, text};
use iced::{Element, Length};

use crate::shortcut::LeaderState;
use crate::theme;

/// Render the status bar at the bottom of the terminal view.
pub fn view<'a, Message: Clone + 'a>(
    host_port: &str,
    session_count: usize,
    leader_state: &LeaderState,
    input_locked: bool,
    term_size: Option<(u16, u16)>,
    on_disconnect: Message,
) -> Element<'a, Message> {
    let mut items: Vec<Element<'a, Message>> = Vec::new();

    // Left: connection info
    items.push(
        text(format!("{host_port} | {session_count} sessions"))
            .size(12)
            .color(theme::FG_DIM)
            .into(),
    );

    items.push(Space::with_width(Length::Fill).into());

    // Center: leader state indicator
    let center_text: Element<'a, Message> = match leader_state {
        LeaderState::AwaitingAction { .. } => {
            text("-- LEADER --").size(12).color(theme::ACCENT).into()
        }
        LeaderState::ConfirmClose { session } => text(format!("Close {session}? (y/n)"))
            .size(12)
            .color(theme::YELLOW)
            .into(),
        LeaderState::SignalMenu { .. } => text("Signal: (k)ill (t)erm (s)top (c)ont")
            .size(12)
            .color(theme::YELLOW)
            .into(),
        LeaderState::HelpVisible => text("Help -- press any key to close")
            .size(12)
            .color(theme::ACCENT)
            .into(),
        LeaderState::CommandPalette { .. } => {
            text("Command Palette").size(12).color(theme::ACCENT).into()
        }
        LeaderState::RenameEditing { .. } => {
            text("Renaming...").size(12).color(theme::ACCENT).into()
        }
        LeaderState::Idle => text("Ctrl+B \u{2192} ? for help")
            .size(12)
            .color(theme::FG_DIM)
            .into(),
    };
    items.push(center_text);

    items.push(Space::with_width(Length::Fill).into());

    // Right: badges
    if input_locked {
        items.push(text(" LOCKED ").size(11).color(theme::YELLOW).into());
    }

    if let Some((rows, cols)) = term_size {
        items.push(
            text(format!("{rows}\u{00d7}{cols}"))
                .size(12)
                .color(theme::FG_DIM)
                .into(),
        );
    }

    items.push(
        button(text("Disconnect").size(12))
            .style(theme::disconnect_button)
            .on_press(on_disconnect)
            .padding([2, 8])
            .into(),
    );

    let bar = row(items)
        .spacing(8)
        .padding([4, 8])
        .width(Length::Fill)
        .align_y(iced::Alignment::Center);

    container(bar)
        .width(Length::Fill)
        .style(theme::status_bar_container)
        .into()
}
