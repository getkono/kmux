use std::time::{Duration, Instant};

use iced::futures::SinkExt as _;
use iced::widget::text_input;
use iced::{Element, Subscription, Theme};
use kmux_protocol::messages::{ClientCapabilities, ServerMessage, TermSize};
use tokio::sync::mpsc;

use kmux_client::connect;
use kmux_client::session_manager::SessionManager;
use kmux_client::token::read_local_token;

use crate::shortcut::LeaderState;

mod key_dispatch;
mod log;
mod message;
mod update;
mod views;

use key_dispatch::keyboard_filter;
pub use message::Message;

/// Connection parameters used as a subscription ID (triggers reconnect on change).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct ConnectParams {
    pub(super) host: String,
    pub(super) port: u16,
    pub(super) token: String,
    pub(super) accept_invalid_certs: bool,
}

/// Which screen is currently shown.
#[derive(Default)]
pub(super) enum Screen {
    #[default]
    Connect,
    Terminal,
}

/// Top-level application state.
#[allow(non_camel_case_types)]
pub struct kmuxApp {
    pub(super) mgr: SessionManager,
    pub(super) screen: Screen,
    pub(super) connect_params: Option<ConnectParams>,

    // Connect form fields (kept for iced text_input widgets)
    pub(super) host: String,
    pub(super) port: String,
    pub(super) token: String,
    pub(super) accept_invalid_certs: bool,

    // Reconnection state
    pub(super) last_connect_params: Option<ConnectParams>,
    pub(super) disconnect_toast: Option<Instant>,

    // Observability
    pub(super) hud_visible: bool,

    // Full-snapshot mode: server sends complete grid snapshots instead of diffs.
    pub(super) force_snapshot_mode: bool,

    // Leader key state machine
    pub(super) leader_state: LeaderState,

    /// Unique ID for this client process, written to the connection log on auth success.
    pub(super) instance_id: String,

    /// Last known terminal dimensions from the canvas widget.
    pub(super) last_term_rows: u16,
    pub(super) last_term_cols: u16,
}

/// Static capability profile for the GUI client.
///
/// - `truecolor`: always true — iced renders with full 24-bit RGB.
/// - `kitty_graphics`/`kitty_keyboard`: false — not yet wired through the cell grid.
fn gui_capabilities() -> ClientCapabilities {
    ClientCapabilities {
        truecolor: true,
        kitty_graphics: false,
        kitty_keyboard: false,
        term: None,
        term_program: Some("kmux-gui".into()),
    }
}

impl kmuxApp {
    pub fn new(accept_invalid_certs: bool, instance_id: String) -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: "8443".to_string(),
            token: read_local_token().unwrap_or_default(),
            accept_invalid_certs,
            mgr: SessionManager::new(
                "127.0.0.1".to_string(),
                8443,
                String::new(),
                accept_invalid_certs,
                gui_capabilities(),
            ),
            screen: Screen::Connect,
            connect_params: None,
            last_connect_params: None,
            disconnect_toast: None,
            hud_visible: false,
            force_snapshot_mode: false,
            leader_state: LeaderState::Idle,
            instance_id,
            last_term_rows: 0,
            last_term_cols: 0,
        }
    }

    /// Get the host:port string for display.
    pub(super) fn host_port_display(&self) -> String {
        self.mgr.host_port_display()
    }

    /// Return the last known terminal size from the canvas, or a default fallback.
    pub(super) fn current_term_size(&self) -> TermSize {
        if self.last_term_rows > 0 && self.last_term_cols > 0 {
            TermSize {
                rows: self.last_term_rows,
                cols: self.last_term_cols,
            }
        } else {
            TermSize::default()
        }
    }

    pub fn view(&self) -> Element<'_, Message> {
        match self.screen {
            Screen::Connect => self.view_connect(),
            Screen::Terminal => self.view_terminal(),
        }
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let Some(params) = self.connect_params.clone() else {
            return Subscription::none();
        };

        let kbd_sub = match self.screen {
            Screen::Terminal => iced::event::listen_with(keyboard_filter),
            Screen::Connect => Subscription::none(),
        };

        let conn_sub = Subscription::run_with_id(
            params.clone(),
            iced::stream::channel::<Message, _>(100, move |mut output| async move {
                let (srv_tx, mut srv_rx) = mpsc::unbounded_channel::<ServerMessage>();

                let result = connect::connect(
                    params.host.clone(),
                    params.port,
                    params.token.clone(),
                    params.accept_invalid_certs,
                    srv_tx,
                    gui_capabilities(),
                    None,
                )
                .await;

                match result {
                    connect::ConnectResult::Connected(sender) => {
                        let _ = output.send(Message::Connected(sender)).await;
                        while let Some(msg) = srv_rx.recv().await {
                            let mut batch = vec![msg];
                            while let Ok(msg) = srv_rx.try_recv() {
                                batch.push(msg);
                            }
                            if output.send(Message::ServerMsgBatch(batch)).await.is_err() {
                                break;
                            }
                        }
                        let _ = output.send(Message::Disconnected).await;
                    }
                    connect::ConnectResult::Failed(e) => {
                        let _ = output.send(Message::ConnectionFailed(e)).await;
                    }
                }
            }),
        );

        // Leader timeout subscription: poll every 100ms when awaiting action
        let leader_sub = if self.leader_state.is_awaiting_action() {
            iced::time::every(Duration::from_millis(100)).map(|_| Message::LeaderTimeout)
        } else {
            Subscription::none()
        };

        Subscription::batch([conn_sub, kbd_sub, leader_sub])
    }

    pub fn theme(&self) -> Theme {
        crate::theme::default()
    }
}

pub(super) fn command_palette_input_id() -> text_input::Id {
    text_input::Id::new("command-palette-input")
}
