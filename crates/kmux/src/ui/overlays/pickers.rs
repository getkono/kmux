use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use crate::app::{App, PickerHits};

use super::{PickerItem, centered_overlay, render_list_picker};

pub fn render_session_picker_overlay(f: &mut Frame, area: Rect, app: &mut App) {
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

    // Index 0 is a synthetic "[+] New session" affordance that transitions
    // to the directory picker on select. Real sessions occupy indices 1..N+1.
    let total_rows = matches.len() + 1;
    let visible = total_rows.min(8);
    let width = 52u16.min(area.width.saturating_sub(4));
    let height = (visible as u16 + 4).min(area.height.saturating_sub(2));
    let overlay_area = centered_overlay(area, width, height);

    let style_for = |is_selected: bool| {
        if is_selected {
            Style::default()
                .fg(theme.bg)
                .bg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.fg).bg(theme.bg)
        }
    };

    let mut items: Vec<PickerItem> = Vec::with_capacity(visible);
    {
        let is_selected = app.session_picker_selected == 0;
        let cursor = if is_selected { ">" } else { " " };
        items.push(PickerItem {
            text: format!("{cursor} [+] New session…"),
            style: style_for(is_selected),
        });
    }
    for (i, entry) in matches.iter().take(visible.saturating_sub(1)).enumerate() {
        let row = i + 1;
        let is_selected = row == app.session_picker_selected;
        let cursor = if is_selected { ">" } else { " " };
        let name = app.mgr.display_name_for(&entry.meta.word_id);
        let pane_count = entry.panes.len();
        items.push(PickerItem {
            text: format!("{cursor} {name:<20} {pane_count}p  {}", entry.meta.cwd),
            style: style_for(is_selected),
        });
    }

    let item_count = items.len();
    let first_row = render_list_picker(
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

    app.picker_hits = picker_hits(overlay_area, first_row, item_count);
}

pub fn render_server_picker_overlay(f: &mut Frame, area: Rect, app: &mut App) {
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

    let item_count = items.len();
    let first_row = render_list_picker(
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

    app.picker_hits = picker_hits(overlay_area, first_row, item_count);
}

pub fn render_dir_picker_overlay(f: &mut Frame, area: Rect, app: &mut App) {
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

    let item_count = items.len();
    let first_row = render_list_picker(
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

    app.picker_hits = picker_hits(overlay_area, first_row, item_count);
}

fn picker_hits(rect: Rect, first_row: Option<u16>, item_count: usize) -> PickerHits {
    let item_rows = match first_row {
        Some(start) => (0..item_count as u16).map(|i| start + i).collect(),
        None => Vec::new(),
    };
    PickerHits {
        rect: Some(rect),
        item_rows,
    }
}
