use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use crate::app::App;

use super::{PickerItem, centered_overlay, render_list_picker};

pub fn render_session_picker_overlay(f: &mut Frame, area: Rect, app: &App) {
    let theme = &app.theme;
    let search = &app.session_picker_search;
    let search_lower = search.to_lowercase();

    let matches: Vec<_> = app
        .mgr
        .session_list()
        .iter()
        .filter(|e| {
            search_lower.is_empty()
                || e.meta.name.to_lowercase().contains(&search_lower)
                || e.meta.word_id.to_lowercase().contains(&search_lower)
        })
        .collect();

    let width = 52u16.min(area.width.saturating_sub(4));
    let height = (matches.len().min(8) as u16 + 4).min(area.height.saturating_sub(2));
    let overlay_area = centered_overlay(area, width, height);

    let items: Vec<PickerItem> = matches
        .iter()
        .take(8)
        .enumerate()
        .map(|(i, entry)| {
            let is_selected = i == app.session_picker_selected;
            let cursor = if is_selected { ">" } else { " " };
            let name = app.mgr.display_name_for(&entry.meta.word_id);
            let pane_count = entry.panes.len();
            PickerItem {
                text: format!("{cursor} {name:<20} {pane_count}p  {}", entry.meta.cwd),
                style: if is_selected {
                    Style::default()
                        .fg(theme.bg)
                        .bg(theme.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.fg).bg(theme.bg)
                },
            }
        })
        .collect();

    render_list_picker(
        f,
        overlay_area,
        theme,
        " Sessions ",
        theme.accent,
        " Search: ",
        search,
        &items,
        " (no results) ",
    );
}

pub fn render_server_picker_overlay(f: &mut Frame, area: Rect, app: &App) {
    let theme = &app.theme;
    let search = &app.server_picker_search;

    let servers = app.filtered_servers();
    let width = 60u16.min(area.width.saturating_sub(4));
    let height = (servers.len().min(8) as u16 + 4).min(area.height.saturating_sub(2));
    let overlay_area = centered_overlay(area, width, height);

    let items: Vec<PickerItem> = servers
        .iter()
        .take(8)
        .enumerate()
        .map(|(i, server)| {
            let is_selected = i == app.server_picker_selected;
            let cursor = if is_selected { ">" } else { " " };
            PickerItem {
                text: format!(
                    "{cursor} {:<28} {}s  {}",
                    server.display,
                    server.sessions.len(),
                    server.time_ago()
                ),
                style: if is_selected {
                    Style::default()
                        .fg(theme.bg)
                        .bg(theme.purple)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.fg).bg(theme.bg)
                },
            }
        })
        .collect();

    render_list_picker(
        f,
        overlay_area,
        theme,
        " Servers ",
        theme.purple,
        " Search: ",
        search,
        &items,
        " (no recent servers) ",
    );
}

pub fn render_dir_picker_overlay(f: &mut Frame, area: Rect, app: &App) {
    let theme = &app.theme;
    let buffer = &app.dir_picker_buffer;

    let matches = app.dir_picker_matches();
    let selected = app.dir_picker_selected.min(matches.len().saturating_sub(1));

    let width = 60u16.min(area.width.saturating_sub(4));
    let height = (matches.len().min(6) as u16 + 5).min(area.height.saturating_sub(2));
    let overlay_area = centered_overlay(area, width, height);

    let items: Vec<PickerItem> = matches
        .iter()
        .take(6)
        .enumerate()
        .map(|(i, entry)| {
            let name = app.mgr.display_name_for(&entry.meta.word_id);
            let marker = if i == selected { ">" } else { " " };
            PickerItem {
                text: format!("{marker} {name:<16} {}", entry.meta.cwd),
                style: Style::default().fg(theme.fg).bg(theme.bg),
            }
        })
        .collect();

    render_list_picker(
        f,
        overlay_area,
        theme,
        " Open Session ",
        theme.accent,
        " Directory: ",
        buffer,
        &items,
        " (no existing sessions — Enter to create new) ",
    );
}
