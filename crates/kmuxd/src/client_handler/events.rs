use kmux_protocol::messages::SessionEventMsg;

/// Translate a kmux-pty lifecycle event into a protocol [`SessionEventMsg`].
///
/// The pty registry uses `pane_id` as the session name.
pub fn pty_event_to_msg(event: kmux_pty::events::SessionEvent) -> SessionEventMsg {
    match event {
        kmux_pty::events::SessionEvent::Spawned { name } => {
            SessionEventMsg::PaneSpawned { pane_id: name }
        }
        kmux_pty::events::SessionEvent::Exited { name, status } => SessionEventMsg::PaneExited {
            pane_id: name,
            code: status.code(),
            signal: match status {
                kmux_pty::process::ExitStatus::Signal(s) => Some(s),
                _ => None,
            },
        },
        kmux_pty::events::SessionEvent::Resized { name, rows, cols } => {
            SessionEventMsg::PaneResized {
                pane_id: name,
                size: kmux_protocol::messages::TermSize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                },
            }
        }
        kmux_pty::events::SessionEvent::Closed { name } => {
            SessionEventMsg::PaneClosed { pane_id: name }
        }
        kmux_pty::events::SessionEvent::Timeout { name, .. } => {
            SessionEventMsg::PaneClosed { pane_id: name }
        }
    }
}
