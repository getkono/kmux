//! Connection state machine shared by the session manager, the TUI badge,
//! and the disconnect overlay.
//!
//! There is exactly one source of truth (`ConnectionState`) to avoid the
//! UI and the manager drifting out of sync. Transitions are driven by:
//!
//! - `SessionManager` (auth handshake, drop, transport swap),
//! - `recovery::Recovery` (handshaking → connected | disconnected),
//! - `liveness::Liveness` (timeout → disconnected).
//!
//! No transport-specific or OS-level signals influence this state — only
//! protocol-layer events.

use std::fmt;

use crate::transport::TransportKind;

/// High-level connection state surfaced to the user.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ConnectionState {
    /// No connection attempt in progress (e.g., after `disconnect()`).
    #[default]
    Idle,
    /// Bootstrap / auth handshake is running.
    Handshaking,
    /// Authenticated and carrying traffic on `transport`.
    Connected { transport: TransportKind },
    /// A drop has happened and `recovery` is actively re-running
    /// `bootstrap_race`. `attempt` starts at 1.
    Reconnecting { attempt: u32 },
    /// Dropped and waiting for the user to confirm a reconnect.
    Disconnected { reason: DisconnectReason },
}

/// Why the connection was lost. Rendered verbatim in the TUI overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisconnectReason {
    /// Server closed its end of the data stream cleanly (or the process died).
    ServerClosed,
    /// No inbound frame for longer than the liveness timeout.
    PingTimeout,
    /// The SSH tunnel process exited while we were using it.
    SshTunnelDied,
    /// Auth failed on a reconnect attempt.
    AuthFailed(String),
    /// `bootstrap_race` returned no successful strategy.
    BootstrapFailed(String),
    /// Explicit user action (e.g. server-picker switch).
    UserInitiated,
    /// Catch-all for transport errors we cannot classify.
    Other(String),
}

impl fmt::Display for DisconnectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DisconnectReason::ServerClosed => write!(f, "server closed connection"),
            DisconnectReason::PingTimeout => write!(f, "ping timeout"),
            DisconnectReason::SshTunnelDied => write!(f, "SSH tunnel died"),
            DisconnectReason::AuthFailed(s) => write!(f, "auth failed: {s}"),
            DisconnectReason::BootstrapFailed(s) => write!(f, "reconnect failed: {s}"),
            DisconnectReason::UserInitiated => write!(f, "disconnected by user"),
            DisconnectReason::Other(s) => write!(f, "{s}"),
        }
    }
}

/// Short label for the status-line badge.
impl ConnectionState {
    pub fn badge_label(&self) -> String {
        match self {
            ConnectionState::Idle => "IDLE".into(),
            ConnectionState::Handshaking => "HANDSHAKING".into(),
            ConnectionState::Connected { transport } => format!("CONNECTED · {transport}"),
            ConnectionState::Reconnecting { attempt } => format!("RECONNECTING #{attempt}"),
            ConnectionState::Disconnected { .. } => "DISCONNECTED".into(),
        }
    }

    pub fn is_live(&self) -> bool {
        matches!(self, ConnectionState::Connected { .. })
    }

    pub fn is_disconnected(&self) -> bool {
        matches!(self, ConnectionState::Disconnected { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badge_labels_cover_all_variants() {
        assert_eq!(ConnectionState::Idle.badge_label(), "IDLE");
        assert_eq!(ConnectionState::Handshaking.badge_label(), "HANDSHAKING");
        assert_eq!(
            ConnectionState::Connected {
                transport: TransportKind::Uds
            }
            .badge_label(),
            "CONNECTED · UDS"
        );
        assert_eq!(
            ConnectionState::Reconnecting { attempt: 3 }.badge_label(),
            "RECONNECTING #3"
        );
        assert_eq!(
            ConnectionState::Disconnected {
                reason: DisconnectReason::PingTimeout,
            }
            .badge_label(),
            "DISCONNECTED"
        );
    }

    #[test]
    fn is_live_only_when_connected() {
        assert!(
            ConnectionState::Connected {
                transport: TransportKind::Quic
            }
            .is_live()
        );
        assert!(!ConnectionState::Idle.is_live());
        assert!(!ConnectionState::Handshaking.is_live());
        assert!(!ConnectionState::Reconnecting { attempt: 1 }.is_live());
        assert!(
            !ConnectionState::Disconnected {
                reason: DisconnectReason::ServerClosed
            }
            .is_live()
        );
    }

    #[test]
    fn reason_display_is_human_readable() {
        assert_eq!(
            DisconnectReason::ServerClosed.to_string(),
            "server closed connection"
        );
        assert_eq!(DisconnectReason::PingTimeout.to_string(), "ping timeout");
        assert_eq!(
            DisconnectReason::AuthFailed("bad token".into()).to_string(),
            "auth failed: bad token"
        );
    }
}
