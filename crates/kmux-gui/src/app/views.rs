use iced::widget::{Space, button, column, container, row, text, text_input};
use iced::{Element, Font, Length, Theme};

use crate::shortcut::{self, LeaderState};
use crate::{session_bar, status_bar, terminal_view, theme};

use super::{Message, command_palette_input_id, kmuxApp};

impl kmuxApp {
    pub(super) fn view_connect(&self) -> Element<'_, Message> {
        let title = text("kmux").size(28).color(theme::ACCENT).font(Font {
            weight: iced::font::Weight::Bold,
            ..Font::MONOSPACE
        });
        let subtitle = text("remote terminal v0.1.0").size(12).color(theme::FG_DIM);

        let status_msg = self.mgr.status_msg();
        let form = column![
            title,
            subtitle,
            Space::with_height(16),
            text("Host").size(13).color(theme::FG),
            text_input("127.0.0.1", &self.host)
                .on_input(Message::HostChanged)
                .style(theme::connect_input),
            text("Port").size(13).color(theme::FG),
            text_input("8443", &self.port)
                .on_input(Message::PortChanged)
                .style(theme::connect_input),
            text("Auth Token").size(13).color(theme::FG),
            text_input("paste token here", &self.token)
                .on_input(Message::TokenChanged)
                .on_submit(Message::ConnectPressed)
                .secure(true)
                .style(theme::connect_input),
            Space::with_height(8),
            button(
                text("Connect")
                    .size(14)
                    .width(Length::Fill)
                    .align_x(iced::alignment::Horizontal::Center)
            )
            .style(theme::connect_button)
            .on_press(Message::ConnectPressed)
            .padding([8, 16])
            .width(Length::Fill),
            {
                let msg_text = text(status_msg).size(12);
                if status_msg.starts_with("Connection failed")
                    || status_msg.starts_with("Auth failed")
                {
                    msg_text.color(theme::RED)
                } else {
                    msg_text.color(theme::FG_DIM)
                }
            },
        ]
        .spacing(6)
        .padding(32)
        .max_width(380);

        let styled_form = container(form).style(theme::connect_container);

        container(styled_form)
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .style(|_theme: &Theme| container::Style {
                background: Some(iced::Background::Color(theme::BG)),
                ..Default::default()
            })
            .into()
    }

    pub(super) fn view_terminal(&self) -> Element<'_, Message> {
        let names: Vec<String> = self
            .mgr
            .session_list()
            .iter()
            .map(|s| s.meta.name.clone())
            .collect();
        let active_ref = self.mgr.active_session();

        let rename_state =
            if let LeaderState::RenameEditing { buffer, session } = &self.leader_state {
                Some((session.as_str(), buffer.as_str()))
            } else {
                None
            };

        let bar = session_bar::view(
            &names,
            active_ref,
            self.leader_state.is_leader_active(),
            rename_state,
            Message::SelectSession,
            Message::CloseSession,
            Message::CreateSessionPressed,
            Message::RenameInput,
            Message::RenameSubmit,
        );

        let (metrics, diag) = if self.hud_visible {
            (
                Some(self.mgr.metrics.snapshot(self.force_snapshot_mode)),
                Some(self.mgr.metrics.diag_snapshot()),
            )
        } else {
            (None, None)
        };

        let terminal_area: Element<Message> = if let Some(name) = self.mgr.active_session() {
            if let Some(buf) = self.mgr.buffer(name) {
                terminal_view::view(buf, name, metrics, diag)
            } else {
                text("No output yet").color(theme::FG_DIM).into()
            }
        } else {
            text("No active session -- press Ctrl+B then c to create one")
                .color(theme::FG_DIM)
                .into()
        };

        let status = status_bar::view(
            &self.host_port_display(),
            self.mgr.session_list().len(),
            &self.leader_state,
            self.mgr.active_input_locked(),
            self.mgr.active_term_size(),
            Message::DisconnectPressed,
        );

        let mut content = column![bar, terminal_area, status]
            .width(Length::Fill)
            .height(Length::Fill);

        // Disconnect toast
        if self.disconnect_toast.is_some() {
            content = content.push(
                container(
                    text("Connection lost \u{2014} reconnecting...")
                        .size(14)
                        .color(iced::Color::WHITE),
                )
                .width(Length::Fill)
                .padding(8)
                .style(theme::toast_error),
            );
        }

        // Overlay: help, command palette
        let base: Element<Message> = content.into();

        match &self.leader_state {
            LeaderState::HelpVisible => {
                let help = self.view_help_overlay();
                iced::widget::stack![base, help].into()
            }
            LeaderState::CommandPalette { query, selected } => {
                let palette = self.view_command_palette(query, *selected);
                iced::widget::stack![base, palette].into()
            }
            _ => base,
        }
    }

    pub(super) fn view_help_overlay(&self) -> Element<'_, Message> {
        let entries = shortcut::shortcut_help_entries();

        let mut col = column![
            text("Keyboard Shortcuts")
                .size(18)
                .color(theme::ACCENT)
                .font(Font {
                    weight: iced::font::Weight::Bold,
                    ..Font::MONOSPACE
                }),
            Space::with_height(12),
        ]
        .spacing(4);

        for (key, desc) in &entries {
            col = col.push(
                row![
                    text(format!("  {key:>10}"))
                        .size(13)
                        .color(theme::GREEN)
                        .font(Font::MONOSPACE),
                    text(format!("  {desc}")).size(13).color(theme::FG),
                ]
                .spacing(8),
            );
        }

        col = col.push(Space::with_height(12));
        col = col.push(text("Press any key to close").size(11).color(theme::FG_DIM));

        let help_box = container(col.padding(24))
            .style(theme::command_palette_container)
            .max_width(480);

        container(
            container(help_box)
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .style(theme::overlay_container)
        .into()
    }

    pub(super) fn view_command_palette(
        &self,
        query: &str,
        selected: usize,
    ) -> Element<'_, Message> {
        let filtered = shortcut::filter_commands(query);

        let input = text_input("Type a command...", query)
            .on_input(Message::CommandPaletteInput)
            .on_submit(Message::CommandPaletteSelect)
            .size(14)
            .style(theme::connect_input)
            .id(command_palette_input_id());

        let mut items_col = column![].spacing(0);
        for (i, entry) in filtered.iter().take(10).enumerate() {
            let is_selected = i == selected;
            let style = if is_selected {
                theme::command_palette_item_selected
            } else {
                theme::command_palette_item
            };
            let text_color = if is_selected {
                iced::Color::WHITE
            } else {
                theme::FG
            };
            let hint_color = if is_selected {
                iced::Color::from_rgba(1.0, 1.0, 1.0, 0.6)
            } else {
                theme::FG_DIM
            };

            let label = entry.label.clone();
            let hint = entry.shortcut_hint.clone();
            let item = container(
                row![
                    text(label).size(13).color(text_color),
                    Space::with_width(Length::Fill),
                    text(hint).size(11).color(hint_color),
                ]
                .padding([4, 8])
                .width(Length::Fill),
            )
            .width(Length::Fill)
            .style(style);

            items_col = items_col.push(item);
        }

        let palette = column![input, items_col]
            .spacing(4)
            .padding(12)
            .max_width(400);

        let palette_box = container(palette).style(theme::command_palette_container);

        // Position at top center
        let positioned = column![
            Space::with_height(60),
            container(palette_box)
                .width(Length::Fill)
                .center_x(Length::Fill),
        ]
        .width(Length::Fill)
        .height(Length::Fill);

        container(positioned)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(theme::overlay_container)
            .into()
    }
}
