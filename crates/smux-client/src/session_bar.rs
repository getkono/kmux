use iced::widget::{button, row, text};
use iced::{Element, Length};

/// Render the session tab bar.
///
/// Each active session gets a tab button. The active session's label is
/// bracketed. A `[+]` button on the right creates a new session.
pub fn view<'a, Message>(
    sessions: &[String],
    active: Option<&str>,
    on_select: impl Fn(String) -> Message + 'a,
    on_new: Message,
) -> Element<'a, Message>
where
    Message: Clone + 'a,
{
    let mut items: Vec<Element<'a, Message>> = sessions
        .iter()
        .map(|name| {
            let is_active = active.map(|a| a == name).unwrap_or(false);
            let label = if is_active {
                format!("[{name}]")
            } else {
                name.clone()
            };
            button(text(label)).on_press(on_select(name.clone())).into()
        })
        .collect();

    items.push(button(text("[+]")).on_press(on_new).into());

    row(items).spacing(4).width(Length::Fill).into()
}
