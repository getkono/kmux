use std::fmt;

/// The active transport channel between client and daemon.
///
/// This enum is **not** serialised in wire messages (only used as `String` in
/// `ChannelSwitched.old_transport`), so adding variants here is source-only and
/// does not require a `PROTOCOL_VERSION` bump.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransportKind {
    /// QUIC/UDP transport (preferred; lower latency, multiplexed streams).
    Quic,
    /// Plain TCP transport (legacy fallback; used only inside SSH tunnels before Phase 4).
    Tcp,
    /// TCP with mandatory TLS (LAN, UDP-blocked internet, SSH `-L` forwarding).
    TcpTls,
    /// Unix domain socket (local same-host IPC; lowest overhead).
    Uds,
}

impl fmt::Display for TransportKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportKind::Quic => write!(f, "QUIC"),
            TransportKind::Tcp => write!(f, "TCP"),
            TransportKind::TcpTls => write!(f, "TCP+TLS"),
            TransportKind::Uds => write!(f, "UDS"),
        }
    }
}

impl TransportKind {
    /// Parse a user-facing transport name (case-insensitive) for the
    /// `/transport` override command (issue #69). Returns `None` for unknown
    /// names; the caller maps `"auto"` to *clearing* the override.
    pub fn parse_cli(s: &str) -> Option<TransportKind> {
        match s.to_ascii_lowercase().as_str() {
            "quic" => Some(TransportKind::Quic),
            "tcp-tls" | "tcptls" | "tls" => Some(TransportKind::TcpTls),
            "tcp" => Some(TransportKind::Tcp),
            "uds" | "unix" | "local" => Some(TransportKind::Uds),
            _ => None,
        }
    }

    /// The override-selectable names shown by the command completer (`auto`
    /// first — it clears the override).
    pub const SELECTABLE_NAMES: &'static [&'static str] =
        &["auto", "quic", "tcp-tls", "uds", "tcp"];
}

/// Current wire protocol version. Bump when the wire format changes.
///
/// The client sends this in `ClientMessage::Auth` and the server rejects
/// connections whose version does not match exactly. Because the wire codec
/// (postcard) is positional, any field addition, removal, or reordering in
/// `ClientMessage` or `ServerMessage` is a breaking change that requires a
/// bump.
///
/// # When to bump
///
/// - Adding, removing, or reordering fields in any message variant.
/// - Adding new enum variants (postcard encodes variant index as a varint).
/// - Changing the semantics of an existing field in a way that old code would
///   misinterpret.
/// - Changing the wire framing (e.g. the per-frame codec tag added in v23 for
///   protocol compression — see [`Compression`] and `docs/compression.md`).
///
/// You do **not** need to bump for purely behavioural changes that leave the
/// wire format unchanged (e.g. changing server-side timeout values).
///
/// # History
///
/// - **23**: per-frame compression — `AuthResult.compression` and the
///   self-describing frame codec tag.
/// - **24**: connection pausing — `ClientMessage::SetPaused` (issue #68).
/// - **25**: daemon federation — `ClientMessage::OpenPeer`/`ClosePeer`,
///   `ServerMessage::PeerOpened`/`PeerClosed`/`PeerError`, and the `PeerTarget`
///   addressing struct (issue #121).
/// - **26**: `PeerTarget` becomes an enum with a `Direct { host, port, token }`
///   TCP+TLS endpoint alongside `Ssh { .. }`, for LAN / same-host federation
///   without SSH (issue #121).
/// - **27**: federated session attribution and peer-routed creation —
///   `SessionEntry.peer` (which machine a listed session lives on) and
///   `ClientMessage::SessionCreate.peer` (create on a connected remote), for the
///   unified session launcher (issue #121).
/// - **28**: OSC 9;4 progress reporting — `PaneInfo.progress_state`/`progress`
///   (carried in the snapshot so late clients see the current bar) and the
///   `SessionEventMsg::PaneProgressChanged` event (issue #125).
pub const PROTOCOL_VERSION: u32 = 28;

/// Wire compression algorithm negotiated for a connection.
///
/// The daemon decides per connection whether to compress (see
/// `docs/compression.md`) and echoes the chosen algorithm in
/// [`ServerMessage::AuthResult`](crate::messages::ServerMessage). Because the
/// exact-match `PROTOCOL_VERSION` handshake already guarantees both peers share
/// an identical codec set, only the *policy* is negotiated, never *support*.
///
/// The level used by the compressor is a sender-side choice and is intentionally
/// not on the wire — the decompressor reconstructs it from the zstd frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Compression {
    /// zstd (RFC 8878). The v1 default; see issue #59.
    Zstd,
}

/// Parse a version-mismatch reason string and return an actionable upgrade
/// hint, or an empty string if the reason is not a version mismatch.
///
/// Expected format: `"protocol version mismatch: client=X, server=Y"`.
pub fn version_mismatch_hint(reason: &str) -> &'static str {
    if let Some(rest) = reason.strip_prefix("protocol version mismatch: client=") {
        let parts: Vec<&str> = rest.splitn(2, ", server=").collect();
        if parts.len() == 2
            && let (Ok(client_v), Ok(server_v)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>())
        {
            return if client_v < server_v {
                "Hint: your client is older than the server. Update kmux to match."
            } else {
                "Hint: your client is newer than the server. Update kmuxd to match."
            };
        }
    }
    ""
}

/// Return the current wall-clock time as milliseconds since the Unix epoch.
pub fn epoch_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Convert a Unix timestamp (seconds since epoch) to `(year, month, day, hour, minute, second)`
/// UTC without any external dependencies.
pub fn epoch_secs_to_ymd_hms(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let days = secs / 86400;
    let time = secs % 86400;
    let h = (time / 3600) as u32;
    let mi = ((time % 3600) / 60) as u32;
    let s = (time % 60) as u32;

    let z = days + 719468;
    let era = z / 146097;
    let doe = z % 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if mo <= 2 { y + 1 } else { y } as u32;
    (y, mo, d, h, mi, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_kind_display_names() {
        assert_eq!(TransportKind::Quic.to_string(), "QUIC");
        assert_eq!(TransportKind::Tcp.to_string(), "TCP");
        assert_eq!(TransportKind::TcpTls.to_string(), "TCP+TLS");
        assert_eq!(TransportKind::Uds.to_string(), "UDS");
    }

    #[test]
    fn parse_cli_accepts_known_names_case_insensitively() {
        assert_eq!(TransportKind::parse_cli("quic"), Some(TransportKind::Quic));
        assert_eq!(TransportKind::parse_cli("QUIC"), Some(TransportKind::Quic));
        assert_eq!(TransportKind::parse_cli("tcp"), Some(TransportKind::Tcp));
        for tls in ["tcp-tls", "tcptls", "tls", "TCP-TLS"] {
            assert_eq!(
                TransportKind::parse_cli(tls),
                Some(TransportKind::TcpTls),
                "{tls}"
            );
        }
        for uds in ["uds", "unix", "local"] {
            assert_eq!(
                TransportKind::parse_cli(uds),
                Some(TransportKind::Uds),
                "{uds}"
            );
        }
    }

    #[test]
    fn parse_cli_rejects_unknown_and_auto() {
        // `auto` is intentionally NOT a transport — the caller maps it to
        // *clearing* the override, so parse_cli must return None for it.
        assert_eq!(TransportKind::parse_cli("auto"), None);
        assert_eq!(TransportKind::parse_cli("bogus"), None);
        assert_eq!(TransportKind::parse_cli(""), None);
    }

    #[test]
    fn version_mismatch_hint_reports_direction_and_boundary() {
        assert!(
            version_mismatch_hint("protocol version mismatch: client=12, server=13")
                .contains("older")
        );
        assert!(
            version_mismatch_hint("protocol version mismatch: client=14, server=13")
                .contains("newer")
        );
        // Boundary: equal versions are not "older" (the comparison is strict <),
        // so the hint falls to the newer-client branch.
        assert!(
            version_mismatch_hint("protocol version mismatch: client=13, server=13")
                .contains("newer")
        );
        // A reason that is not a version mismatch yields no hint.
        assert_eq!(version_mismatch_hint("connection refused"), "");
        // Malformed (unparseable) versions also yield no hint.
        assert_eq!(
            version_mismatch_hint("protocol version mismatch: client=x, server=y"),
            ""
        );
    }

    #[test]
    fn epoch_millis_is_well_past_2020() {
        // 1_577_836_800_000 ms == 2020-01-01T00:00:00Z. The wall clock is long
        // past that, which pins the function against the `-> 0` / `-> 1` mutants.
        assert!(epoch_millis() > 1_577_836_800_000);
    }

    #[test]
    fn epoch_secs_to_ymd_hms_matches_known_timestamps() {
        // Unix epoch.
        assert_eq!(epoch_secs_to_ymd_hms(0), (1970, 1, 1, 0, 0, 0));
        // One day / one (non-leap) year later: date rollover.
        assert_eq!(epoch_secs_to_ymd_hms(86_400), (1970, 1, 2, 0, 0, 0));
        assert_eq!(epoch_secs_to_ymd_hms(31_536_000), (1971, 1, 1, 0, 0, 0));
        // A known wall-clock instant: 2023-11-14T22:13:20Z.
        assert_eq!(
            epoch_secs_to_ymd_hms(1_700_000_000),
            (2023, 11, 14, 22, 13, 20)
        );
        // Leap day exercises the civil-calendar month/day branch.
        assert_eq!(epoch_secs_to_ymd_hms(1_582_934_400), (2020, 2, 29, 0, 0, 0));
    }
}
