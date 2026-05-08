use ratatui::Frame;

use crate::app::App;
use crate::mode::Mode;

mod bars;
mod connect;
mod grid;
mod overlays;

pub fn render(f: &mut Frame, app: &mut App) {
    match app.mode.clone() {
        Mode::Connect { field } => connect::render_connect(f, app, &field),
        _ => render_terminal(f, app),
    }
}

fn render_terminal(f: &mut Frame, app: &mut App) {
    use ratatui::layout::{Constraint, Layout};

    let area = f.area();

    // Layout: session bar (1) | terminal (fill) | status bar (1) | hint bar (1)
    let chunks = Layout::vertical([
        Constraint::Length(1), // session bar
        Constraint::Min(1),    // terminal
        Constraint::Length(1), // status bar
        Constraint::Length(1), // hint bar
    ])
    .split(area);

    bars::render_session_bar(f, app, chunks[0]);
    grid::render_grid(f, app, chunks[1]);
    bars::render_status_bar(f, app, chunks[2]);
    bars::render_hint_bar(f, app, chunks[3]);

    // Drop stale picker hit-boxes whenever no picker is being rendered this frame;
    // the picker render fns overwrite this below if one is active.
    if !matches!(
        app.mode,
        Mode::SessionPicker | Mode::ServerPicker | Mode::DirectoryPicker,
    ) {
        app.picker_hits = crate::app::PickerHits::default();
    }

    // Overlays — clone mode to avoid borrow conflict with app
    let mode_snap = app.mode.clone();
    match &mode_snap {
        Mode::Help => overlays::render_help_overlay(f, area, &app.theme),
        Mode::ConfirmCloseSession { word_id } => {
            overlays::render_confirm_overlay(f, area, word_id, &app.theme)
        }
        Mode::RenameSession { buffer, word_id } => {
            overlays::render_rename_overlay(f, area, word_id, buffer, &app.theme)
        }
        Mode::SessionPicker => overlays::render_session_picker_overlay(f, area, app),
        Mode::ServerPicker => overlays::render_server_picker_overlay(f, area, app),
        Mode::DirectoryPicker => overlays::render_dir_picker_overlay(f, area, app),
        Mode::Connecting { target_display } => {
            // Defensive: if the connection is already live, the mode is stale
            // and the overlay would spuriously cover the live grid. The
            // bootstrap_result arm should have cleared Connecting, but an async
            // race could leave the mode lagging behind the state machine for a
            // frame or two — skip the overlay rather than smudge the terminal.
            if !app.mgr.connection_state().is_live() {
                overlays::render_connecting_overlay(f, area, &app.theme, target_display);
            }
        }
        Mode::Disconnected { reason } => {
            overlays::render_disconnect_overlay(f, area, &app.theme, reason)
        }
        Mode::Command(_) => overlays::render_command_overlay(f, area, app),
        _ => {}
    }

    // HUD overlay
    if app.hud_visible {
        overlays::render_hud(f, app, chunks[1]);
    }

    // Metrics overlay (toggled via Ctrl+G m).
    if app.metrics_overlay_visible {
        overlays::render_metrics_overlay(f, area, app);
    }
}
