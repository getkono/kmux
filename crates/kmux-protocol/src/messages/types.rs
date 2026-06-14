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
pub const PROTOCOL_VERSION: u32 = 23;

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
