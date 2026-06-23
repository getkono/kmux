use serde::{Deserialize, Serialize};

use crate::dirs::BuildProfile;
use crate::messages::SessionMeta;

/// Canonical argv appended when the client spawns a local daemon.
///
/// Any site that shells out to `kmuxd` must use this constant so that the
/// args, bind address, and port are never duplicated or allowed to drift.
///
/// Binds to `0.0.0.0` (not `127.0.0.1`) because remote daemons spawned over
/// SSH must accept QUIC datagrams arriving on the host's external interface
/// — a loopback bind makes QUIC unreachable to any non-local client and
/// silently locks in TCP+TLS forever (TCP+TLS happens to work despite a
/// loopback bind because the SSH `-L` tunnel terminates on the remote's
/// loopback; QUIC has no such tunnel).
pub const DAEMON_BOOT_ARGS: &[&str] = &["--daemon", "--bind", "0.0.0.0", "--port", "0"];

/// Version of the daemon-to-daemon handoff protocol.
///
/// A graceful restart streams live PTY master file descriptors from the
/// outgoing daemon to the incoming one over a Unix socket (`SCM_RIGHTS`). The
/// two daemons may be different builds during an upgrade, so — like every other
/// cross-component boundary in kmux — the handoff is versioned. The incoming
/// daemon refuses the live-fd transfer on a mismatch and falls back to the
/// (already versioned) on-disk snapshot restore, which is always safe.
///
/// Bump this on ANY change to the [`HandoffMessage`] wire format.
pub const HANDOFF_PROTOCOL_VERSION: u32 = 1;

/// JSON request sent to the daemon control socket.
#[derive(Deserialize)]
pub struct ControlRequest {
    pub command: String,
}

/// JSON response to the `"status"` control command.
#[derive(Serialize, Deserialize)]
pub struct StatusResponse {
    pub status: String,
    pub port: u16,
    #[serde(default)]
    pub tcp_port: u16,
    pub token: String,
    pub pid: u32,
    #[serde(default)]
    pub uptime_secs: u64,
    #[serde(default)]
    pub session_count: usize,
    #[serde(default)]
    pub protocol_version: u32,
    #[serde(default)]
    pub kmuxd_version: String,
    /// Cargo profile `kmuxd` was compiled with.
    ///
    /// `None` only when the peer predates this field — the client treats that
    /// as a refused handshake because an unknown profile cannot be verified.
    #[serde(default)]
    pub build_profile: Option<BuildProfile>,
    #[serde(default)]
    pub endpoints: Vec<EndpointEntry>,
}

/// An advertised transport endpoint in a `StatusResponse`.
#[derive(Serialize, Deserialize, Clone)]
pub struct EndpointEntry {
    pub kind: String,
    pub address: String,
}

/// JSON response to the `"stop"` control command.
#[derive(Serialize, Deserialize)]
pub struct StopResponse {
    pub status: String,
}

/// JSON response to the `"restart"` control command.
///
/// `handoff` is `true` when the daemon initiated a graceful live-PTY handoff to
/// a successor instance. A daemon old enough to predate this command closes the
/// connection without responding, so the client treats the resulting read error
/// as "unsupported" and falls back to a hard stop-then-respawn restart.
#[derive(Serialize, Deserialize)]
pub struct RestartResponse {
    pub status: String,
    pub handoff: bool,
}

/// Per-pane metadata sent in [`HandoffMessage::Hello`].
///
/// The fd itself is delivered out-of-band as `SCM_RIGHTS` ancillary data on a
/// later [`HandoffMessage::PaneFd`]; everything else the successor needs to
/// rebuild the pane (program, size, scrollback, grid) comes from the on-disk
/// checkpoint, keyed by `pane_id`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct HandoffPaneMeta {
    /// `{word_id}/{pane_index}` — the registry key shared with the checkpoint.
    pub pane_id: String,
    /// Child PID (informational; used by the successor for foreign-child
    /// liveness polling, since a reparented child cannot be `waitpid`-ed).
    pub pid: i32,
    /// Whether a live master fd will be streamed for this pane. When `false`
    /// (the child already exited), the successor respawns it from the snapshot.
    pub has_live_fd: bool,
}

/// Daemon-to-daemon handoff control frames, exchanged as `\n`-delimited JSON
/// over the handoff Unix socket. The only payload carried out-of-band (as
/// `SCM_RIGHTS` ancillary data) is the PTY master fd on [`HandoffMessage::PaneFd`].
///
/// O = outgoing daemon, N = incoming (successor) daemon.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum HandoffMessage {
    /// O → N: opening frame. Lists every pane (in fd-stream order) and the auth
    /// token for N to adopt so already-connected clients re-auth seamlessly.
    Hello {
        version: u32,
        token: String,
        panes: Vec<HandoffPaneMeta>,
    },
    /// N → O: N speaks the same handoff version and will pull the live fds.
    Accept,
    /// N → O: N declines the live transfer (e.g. version mismatch) and will fall
    /// back to snapshot restore. O lets its children exit normally.
    Decline { reason: String },
    /// O → N: one live PTY master fd for `pane_id` follows as ancillary data.
    PaneFd { pane_id: String },
    /// N → O: the preceding [`PaneFd`](Self::PaneFd) was received and adopted.
    /// Keeps fd streaming lock-step so each frame carries exactly one fd.
    PaneFdAck,
    /// O → N: every live fd has been streamed.
    Complete,
    /// N → O: N has reconstructed all panes and bound its sockets; O may exit.
    Ack,
    /// O → N: O has released its sockets and is exiting (informational).
    Released,
}

/// JSON response to the `"sessions"` control command.
#[derive(Serialize, Deserialize)]
pub struct SessionsResponse {
    pub sessions: Vec<SessionConnections>,
    /// Auth'd connections that are not attached to any session pane.
    pub unattached: Vec<ConnectionInfo>,
}

/// A session and the connections attached to any of its panes.
#[derive(Serialize, Deserialize)]
pub struct SessionConnections {
    pub meta: SessionMeta,
    pub panes_count: usize,
    pub connections: Vec<ConnectionInfo>,
}

/// Per-connection telemetry snapshot.
#[derive(Serialize, Deserialize, Clone)]
pub struct ConnectionInfo {
    pub connection_id: u64,
    pub client_id: u64,
    pub transport: String,
    /// Daemon-assigned user-readable label `username@hostname[#N]` (issue #146).
    #[serde(default)]
    pub label: String,
    /// Cryptographic machine/user identity: hex SHA-256 of the public key
    /// (issue #146). Empty for connections from pre-identity clients.
    #[serde(default)]
    pub machine_id: String,
    /// Client-reported hostname (friendly label only) (issue #146).
    #[serde(default)]
    pub hostname: String,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub msgs_in: u64,
    pub msgs_out: u64,
    /// Seconds since this connection was registered.
    pub uptime_secs: u64,
    /// Milliseconds since any inbound frame (None if no frame ever received).
    #[serde(default)]
    pub last_activity_ago_ms: Option<u64>,
    /// Milliseconds since the last successful ping/pong round-trip (None if never).
    #[serde(default)]
    pub last_pong_ago_ms: Option<u64>,
    /// Most recent ping RTT in milliseconds (None if never measured).
    #[serde(default)]
    pub last_rtt_ms: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // The control socket speaks newline-delimited JSON between a client and a
    // daemon that may be *different builds* during an upgrade. These tests pin
    // that wire format: a field rename, a dropped `#[serde(default)]`, or a
    // change to the enum tagging would silently break cross-version control.

    #[test]
    fn control_request_parses_status_command() {
        let req: ControlRequest =
            serde_json::from_str(r#"{"command":"status"}"#).expect("valid control request");
        assert_eq!(req.command, "status");
    }

    #[test]
    fn status_response_round_trips_through_json() {
        let resp = StatusResponse {
            status: "running".into(),
            port: 8443,
            tcp_port: 8444,
            token: "secret".into(),
            pid: 4242,
            uptime_secs: 90,
            session_count: 3,
            protocol_version: 23,
            kmuxd_version: "0.2.0".into(),
            build_profile: None,
            endpoints: vec![EndpointEntry {
                kind: "quic".into(),
                address: "1.2.3.4:8443".into(),
            }],
        };

        let json = serde_json::to_string(&resp).expect("serialize");
        let back: StatusResponse = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(back.status, "running");
        assert_eq!(back.port, 8443);
        assert_eq!(back.tcp_port, 8444);
        assert_eq!(back.token, "secret");
        assert_eq!(back.pid, 4242);
        assert_eq!(back.uptime_secs, 90);
        assert_eq!(back.session_count, 3);
        assert_eq!(back.protocol_version, 23);
        assert_eq!(back.kmuxd_version, "0.2.0");
        assert!(back.build_profile.is_none());
        assert_eq!(back.endpoints.len(), 1);
        assert_eq!(back.endpoints[0].kind, "quic");
        assert_eq!(back.endpoints[0].address, "1.2.3.4:8443");
    }

    #[test]
    fn status_response_defaults_fields_an_older_daemon_omits() {
        // A daemon predating the richer telemetry sends only the original four
        // required fields. The `#[serde(default)]` fields must fill in — and
        // crucially `build_profile` must be `None`, which the client treats as
        // "unverifiable, refuse the handshake", never a spurious value.
        let minimal = r#"{"status":"running","port":8443,"token":"t","pid":1}"#;
        let resp: StatusResponse = serde_json::from_str(minimal).expect("deserialize minimal");

        assert_eq!(resp.tcp_port, 0);
        assert_eq!(resp.uptime_secs, 0);
        assert_eq!(resp.session_count, 0);
        assert_eq!(resp.protocol_version, 0);
        assert_eq!(resp.kmuxd_version, "");
        assert!(resp.build_profile.is_none());
        assert!(resp.endpoints.is_empty());
    }

    #[test]
    fn restart_response_round_trips_handoff_flag() {
        for handoff in [true, false] {
            let json = serde_json::to_string(&RestartResponse {
                status: "ok".into(),
                handoff,
            })
            .expect("serialize");
            let back: RestartResponse = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back.status, "ok");
            assert_eq!(back.handoff, handoff);
        }
    }

    #[test]
    fn handoff_hello_round_trips_with_pane_metadata() {
        let hello = HandoffMessage::Hello {
            version: HANDOFF_PROTOCOL_VERSION,
            token: "tok".into(),
            panes: vec![HandoffPaneMeta {
                pane_id: "eagle/0".into(),
                pid: 4242,
                has_live_fd: true,
            }],
        };
        let json = serde_json::to_string(&hello).expect("serialize");
        let back: HandoffMessage = serde_json::from_str(&json).expect("deserialize");
        match back {
            HandoffMessage::Hello {
                version,
                token,
                panes,
            } => {
                assert_eq!(version, HANDOFF_PROTOCOL_VERSION);
                assert_eq!(token, "tok");
                assert_eq!(panes.len(), 1);
                assert_eq!(panes[0].pane_id, "eagle/0");
                assert_eq!(panes[0].pid, 4242);
                assert!(panes[0].has_live_fd);
            }
            other => panic!("expected Hello, got {other:?}"),
        }
    }

    #[test]
    fn handoff_frames_use_external_tagging() {
        // The lock-step fd-streaming handshake depends on serde's default
        // external tagging: unit variants are bare strings, struct variants are
        // single-key objects. Pin it so an attribute change can't desync the
        // outgoing/incoming daemons mid-upgrade.
        assert_eq!(
            serde_json::to_string(&HandoffMessage::Accept).expect("serialize"),
            "\"Accept\""
        );
        assert_eq!(
            serde_json::to_string(&HandoffMessage::PaneFdAck).expect("serialize"),
            "\"PaneFdAck\""
        );
        assert_eq!(
            serde_json::to_string(&HandoffMessage::Decline {
                reason: "version mismatch".into(),
            })
            .expect("serialize"),
            r#"{"Decline":{"reason":"version mismatch"}}"#
        );

        // The remaining frames must survive a full round-trip unchanged.
        for msg in [
            HandoffMessage::PaneFd {
                pane_id: "eagle/0".into(),
            },
            HandoffMessage::Complete,
            HandoffMessage::Ack,
            HandoffMessage::Released,
        ] {
            let json = serde_json::to_string(&msg).expect("serialize");
            serde_json::from_str::<HandoffMessage>(&json).expect("round-trip");
        }
    }
}
