use std::fmt;

/// The active transport channel between client and daemon.
///
/// This enum is **not** serialised in wire messages (only used as `String` in
/// `ChannelSwitched.old_transport`), so adding variants here is source-only and
/// does not touch [`PROTOCOL_RANGE`].
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
            Self::Quic => write!(f, "QUIC"),
            Self::Tcp => write!(f, "TCP"),
            Self::TcpTls => write!(f, "TCP+TLS"),
            Self::Uds => write!(f, "UDS"),
        }
    }
}

impl TransportKind {
    /// Parse a user-facing transport name (case-insensitive) for the
    /// `/transport` override command (issue #69). Returns `None` for unknown
    /// names; the caller maps `"auto"` to *clearing* the override.
    pub fn parse_cli(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "quic" => Some(Self::Quic),
            "tcp-tls" | "tcptls" | "tls" => Some(Self::TcpTls),
            "tcp" => Some(Self::Tcp),
            "uds" | "unix" | "local" => Some(Self::Uds),
            _ => None,
        }
    }

    /// The override-selectable names shown by the command completer (`auto`
    /// first — it clears the override).
    pub const SELECTABLE_NAMES: &'static [&'static str] =
        &["auto", "quic", "tcp-tls", "uds", "tcp"];
}

/// Semantic version of the named MessagePack data-plane schema.
///
/// Major versions are breaking. Minor versions may add defaulted fields or
/// capability-gated messages. Patch versions change no schema semantics. Normal
/// additive feature work advertises a named capability and therefore does not
/// edit this shared constant; range changes are deliberate baseline-policy
/// changes.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
}

impl ProtocolVersion {
    pub const fn new(major: u16, minor: u16, patch: u16) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Inclusive protocol-version range supported by a binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProtocolRange {
    pub min: ProtocolVersion,
    pub max: ProtocolVersion,
}

impl ProtocolRange {
    /// A range supporting exactly one version.
    pub const fn exact(version: ProtocolVersion) -> Self {
        Self {
            min: version,
            max: version,
        }
    }

    /// Highest version both ranges support, or `None` when they cannot speak.
    ///
    /// A range must stay within a single major version: majors are, by
    /// definition, mutually unintelligible schemas, so a binary cannot claim to
    /// speak two of them over one connection. A cross-major range is therefore
    /// treated as unusable rather than silently narrowed.
    pub fn negotiate(self, other: Self) -> Option<ProtocolVersion> {
        if self.min.major != self.max.major
            || other.min.major != other.max.major
            || self.max.major != other.max.major
        {
            return None;
        }
        let min = self.min.max(other.min);
        let max = self.max.min(other.max);
        (min <= max).then_some(max)
    }
}

impl fmt::Display for ProtocolRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.min == self.max {
            self.min.fmt(f)
        } else {
            write!(f, "{}..={}", self.min, self.max)
        }
    }
}

/// Newest schema version this build speaks (the top of [`PROTOCOL_RANGE`]).
pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::new(1, 0, 0);
/// Oldest schema version this build still accepts from a peer.
pub const MIN_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::new(1, 0, 0);
/// The range advertised in `Auth` and matched against the peer's.
///
/// Adding a feature does **not** belong here: use a defaulted field or a named
/// capability. Widening or moving this range is a deliberate baseline-policy
/// change — see `docs/architecture-protocol-versioning.md`.
pub const PROTOCOL_RANGE: ProtocolRange = ProtocolRange {
    min: MIN_PROTOCOL_VERSION,
    max: PROTOCOL_VERSION,
};

/// Successor to the retired monotonic `PROTOCOL_VERSION: u32` (which ended at
/// 40), frozen at 41 and never used for a compatibility decision.
///
/// It stays in the JSON status/probe output for two reasons: consumers written
/// against the old integer field keep parsing, and a protocol-40 peer reads 41
/// as "newer than me" and refuses — instead of decoding a named-map frame with
/// a positional Postcard decoder.
pub const LEGACY_PROTOCOL_VERSION: u32 = 41;

/// Per-frame zstd compression of the wire codec (frame codec tag 3).
pub const CAPABILITY_FRAME_ZSTD: &str = "frame.zstd";
/// Every optional capability this build implements.
///
/// Extending this list is the normal way to ship an optional protocol feature:
/// it is additive, needs no version bump, and appending a name conflicts far
/// less than editing a shared integer.
pub const PROTOCOL_CAPABILITIES: &[&str] = &[CAPABILITY_FRAME_ZSTD];

/// Capabilities to offer in `ClientMessage::Auth`.
pub fn protocol_capabilities() -> Vec<String> {
    PROTOCOL_CAPABILITIES
        .iter()
        .map(|capability| (*capability).to_string())
        .collect()
}

/// Intersect a peer's offered capabilities with ours.
///
/// Names we do not know are ignored, never rejected: an older peer must be able
/// to talk to a newer one that offers extensions it has never heard of.
pub fn negotiate_capabilities(offered: &[String]) -> Vec<String> {
    PROTOCOL_CAPABILITIES
        .iter()
        .filter(|ours| offered.iter().any(|theirs| theirs == *ours))
        .map(|capability| (*capability).to_string())
        .collect()
}

/// Wire compression algorithm negotiated for a connection.
///
/// The daemon decides per connection whether to compress (see
/// `docs/compression.md`) and echoes the chosen algorithm in
/// [`ServerMessage::AuthResult`](crate::messages::ServerMessage). The
/// [`CAPABILITY_FRAME_ZSTD`] handshake establishes that both peers *support* the
/// codec; this enum then carries the daemon's per-connection *policy*.
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
    if reason.starts_with("protocol version mismatch:") {
        "Hint: update kmux and kmuxd until their supported protocol ranges overlap."
    } else if reason.starts_with("legacy protocol version:") {
        "Hint: update the legacy kmux or kmuxd binary before connecting."
    } else {
        ""
    }
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
    fn version_mismatch_hint_classifies_the_failure() {
        let hint = version_mismatch_hint("protocol version mismatch: client=1.0.0, server=2.0.0");
        assert!(hint.contains("ranges overlap"));
        // A reason that is not a version mismatch yields no hint.
        assert_eq!(version_mismatch_hint("connection refused"), "");
        // Formatting details do not suppress the safe range-overlap guidance.
        assert!(
            version_mismatch_hint("protocol version mismatch: client=x, server=y")
                .contains("ranges overlap")
        );
    }

    #[test]
    fn protocol_range_negotiates_highest_overlap() {
        let ours = ProtocolRange {
            min: ProtocolVersion::new(1, 1, 0),
            max: ProtocolVersion::new(1, 4, 0),
        };
        let theirs = ProtocolRange {
            min: ProtocolVersion::new(1, 2, 0),
            max: ProtocolVersion::new(1, 3, 0),
        };
        assert_eq!(ours.negotiate(theirs), Some(ProtocolVersion::new(1, 3, 0)));
    }

    #[test]
    fn protocol_range_rejects_disjoint_and_cross_major_ranges() {
        let current = ProtocolRange::exact(ProtocolVersion::new(1, 0, 0));
        assert_eq!(
            current.negotiate(ProtocolRange::exact(ProtocolVersion::new(1, 1, 0))),
            None
        );
        assert_eq!(
            current.negotiate(ProtocolRange::exact(ProtocolVersion::new(2, 0, 0))),
            None
        );
    }

    #[test]
    fn capability_negotiation_ignores_unknown_extensions() {
        let offered = vec![
            CAPABILITY_FRAME_ZSTD.to_string(),
            "future.example".to_string(),
        ];
        assert_eq!(negotiate_capabilities(&offered), protocol_capabilities());
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
