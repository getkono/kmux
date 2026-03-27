use iced::widget::{button, container, row, text, text_input};
use iced::{Element, Length};

use crate::theme;

/// Render the session tab bar with indexed tabs, close buttons, and inline rename.
#[allow(clippy::too_many_arguments)]
pub fn view<'a, Message>(
    sessions: &[String],
    active: Option<&str>,
    leader_active: bool,
    rename_state: Option<(&str, &str)>, // (session, current_input)
    on_select: impl Fn(String) -> Message + 'a,
    on_close: impl Fn(String) -> Message + 'a,
    on_new: Message,
    on_rename_input: impl Fn(String) -> Message + Clone + 'a,
    on_rename_submit: Message,
) -> Element<'a, Message>
where
    Message: Clone + 'a,
{
    let mut items: Vec<Element<'a, Message>> = Vec::new();

    for (i, name) in sessions.iter().enumerate() {
        let is_active = active == Some(name.as_str());
        let display_idx = if i == 9 { 0 } else { i + 1 };

        // Check if this tab is being renamed
        if let Some((rename_session, rename_buf)) = rename_state
            && rename_session == name
        {
            let rename_fn = on_rename_input.clone();
            let input = text_input("new name", rename_buf)
                .on_input(rename_fn)
                .on_submit(on_rename_submit.clone())
                .size(13)
                .width(Length::Fixed(140.0))
                .style(theme::connect_input)
                .id(rename_input_id());
            items.push(input.into());
            continue;
        }

        let label = format!("{display_idx}: {name}");
        let tab_style = if is_active {
            theme::tab_active
        } else {
            theme::tab_inactive
        };

        let tab_btn = button(text(label).size(13))
            .style(tab_style)
            .on_press(on_select(name.clone()))
            .padding([4, 8]);

        let close_btn = button(text("\u{00d7}").size(13))
            .style(theme::tab_close)
            .on_press(on_close(name.clone()))
            .padding([4, 4]);

        items.push(
            row![tab_btn, close_btn]
                .spacing(0)
                .align_y(iced::Alignment::Center)
                .into(),
        );
    }

    // Spacer to push [+] to the right
    items.push(iced::widget::Space::with_width(Length::Fill).into());

    // New session button
    items.push(
        button(text("+").size(13))
            .style(theme::tab_new)
            .on_press(on_new)
            .padding([4, 8])
            .into(),
    );

    let bar = row(items)
        .spacing(2)
        .padding([4, 8])
        .width(Length::Fill)
        .align_y(iced::Alignment::Center);

    let bar_style = if leader_active {
        theme::leader_active_bar
    } else {
        theme::session_bar_container
    };

    container(bar).width(Length::Fill).style(bar_style).into()
}

/// The text_input ID used for the rename field, so we can focus it programmatically.
pub fn rename_input_id() -> text_input::Id {
    text_input::Id::new("session-rename-input")
}
