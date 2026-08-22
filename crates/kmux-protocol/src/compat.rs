//! Single source of truth for client↔daemon compatibility classification.
//!
//! Three call sites used to re-spell the same protocol/profile/build-fingerprint
//! comparisons: `ensure_compatible_daemon` (the attach gate), `kmux daemon
//! status`, and `kmux client status` (and now `kmux status`). This module owns
//! the *decision* — what counts as same / different / unknown, and what is
//! blocking — while each caller keeps formatting its own user-facing prose.
//!
//! The reference is always *this* binary: [`protocol_match`] compares against
//! [`PROTOCOL_RANGE`] and [`profile_match`] against [`BuildProfile::CURRENT`],
//! both linked into whatever binary calls this.

use crate::messages::{PROTOCOL_RANGE, ProtocolRange, ProtocolVersion};

/// Cargo build profile of a kmux binary.
///
/// Advertised by `kmuxd` in its status response and checked by `kmux` during
/// the control-socket handshake: a mismatch means the client and the daemon
/// resolved different runtime dirs (`kmux-debug/` vs `kmux/`), so the client
/// would have silently attached to the wrong instance — we refuse.
#[derive(Debug, Clone, Copy, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BuildProfile {
    Debug,
    Release,
}

impl BuildProfile {
    /// Profile the current crate was compiled with.
    #[cfg(debug_assertions)]
    pub const CURRENT: Self = Self::Debug;
    #[cfg(not(debug_assertions))]
    pub const CURRENT: Self = Self::Release;

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Release => "release",
        }
    }
}

impl std::fmt::Display for BuildProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Three-way comparison of a peer's reported field against ours.
///
/// `Unknown` means the peer omitted the field — `None` for a protocol range or
/// build profile, or an empty build fingerprint. Whether it blocks depends on
/// the field: an absent protocol range is a legacy wire format and cannot be
/// decoded, while an absent fingerprint remains advisory.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Match3 {
    Same,
    Differ,
    Unknown,
}

/// Compare a peer's supported schema range against ours.
///
/// [`Match3::Same`] means *compatible*, not identical: the ranges overlap, so
/// the two can settle on a shared version. `None` is a legacy daemon that only
/// reported the retired monotonic integer.
pub fn protocol_match(theirs: Option<ProtocolRange>) -> Match3 {
    match theirs {
        None => Match3::Unknown,
        Some(range) if PROTOCOL_RANGE.negotiate(range).is_some() => Match3::Same,
        Some(_) => Match3::Differ,
    }
}

/// Highest schema baseline supported by both peers.
pub fn negotiate_protocol(theirs: ProtocolRange) -> Option<ProtocolVersion> {
    PROTOCOL_RANGE.negotiate(theirs)
}

/// Compare a peer's build profile against ours. `None` (a daemon predating the
/// field) is [`Match3::Unknown`].
pub fn profile_match(theirs: Option<BuildProfile>) -> Match3 {
    match theirs {
        None => Match3::Unknown,
        Some(p) if p == BuildProfile::CURRENT => Match3::Same,
        Some(_) => Match3::Differ,
    }
}

/// Compare a peer's build fingerprint against `ours`. An empty `theirs` (a
/// daemon/client predating build reporting) is [`Match3::Unknown`].
pub fn build_match(theirs: &str, ours: &str) -> Match3 {
    if theirs.is_empty() {
        Match3::Unknown
    } else if theirs == ours {
        Match3::Same
    } else {
        Match3::Differ
    }
}

/// Why a local connection to a daemon is refused.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum BlockReason {
    /// The supported protocol ranges do not overlap.
    Protocol,
    /// The peer predates semantic range negotiation and speaks Postcard.
    ProtocolUnknown,
    /// Debug vs release: they resolved different runtime dirs, so they never
    /// share a socket — attaching would target the wrong instance.
    ProfileMismatch,
    /// The daemon did not report a build profile, so we cannot verify it
    /// matches — refused as unverifiable.
    ProfileUnknown,
}

/// The attach-gate policy enforced by `ensure_compatible_daemon` and surfaced
/// (as the exit code) by `kmux daemon status` / `kmux status`.
///
/// Blocking: a disjoint or absent protocol range, or a profile that differs or
/// is unknown. Protocol takes precedence over profile because its message is
/// more actionable. Build-fingerprint skew remains advisory.
pub fn attach_block(
    protocol: Option<ProtocolRange>,
    profile: Option<BuildProfile>,
) -> Option<BlockReason> {
    match protocol_match(protocol) {
        Match3::Differ => return Some(BlockReason::Protocol),
        Match3::Unknown => return Some(BlockReason::ProtocolUnknown),
        Match3::Same => {}
    }
    match profile_match(profile) {
        Match3::Same => None,
        Match3::Differ => Some(BlockReason::ProfileMismatch),
        Match3::Unknown => Some(BlockReason::ProfileUnknown),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The opposite profile to whatever the test binary was compiled with, so
    /// these tests are profile-agnostic.
    fn other_profile() -> BuildProfile {
        match BuildProfile::CURRENT {
            BuildProfile::Debug => BuildProfile::Release,
            BuildProfile::Release => BuildProfile::Debug,
        }
    }

    #[test]
    fn protocol_match_classifies() {
        assert_eq!(protocol_match(None), Match3::Unknown);
        assert_eq!(protocol_match(Some(PROTOCOL_RANGE)), Match3::Same);
        assert_eq!(
            protocol_match(Some(ProtocolRange::exact(ProtocolVersion::new(2, 0, 0)))),
            Match3::Differ
        );
    }

    #[test]
    fn profile_match_classifies() {
        assert_eq!(profile_match(None), Match3::Unknown);
        assert_eq!(profile_match(Some(BuildProfile::CURRENT)), Match3::Same);
        assert_eq!(profile_match(Some(other_profile())), Match3::Differ);
    }

    #[test]
    fn build_match_classifies() {
        assert_eq!(build_match("", "abc"), Match3::Unknown);
        assert_eq!(build_match("abc", "abc"), Match3::Same);
        assert_eq!(build_match("abc", "def"), Match3::Differ);
    }

    #[test]
    fn attach_block_allows_a_matching_daemon() {
        assert_eq!(
            attach_block(Some(PROTOCOL_RANGE), Some(BuildProfile::CURRENT)),
            None
        );
    }

    #[test]
    fn attach_block_refuses_unknown_protocol() {
        assert_eq!(
            attach_block(None, Some(BuildProfile::CURRENT)),
            Some(BlockReason::ProtocolUnknown)
        );
    }

    #[test]
    fn attach_block_refuses_protocol_mismatch() {
        assert_eq!(
            attach_block(
                Some(ProtocolRange::exact(ProtocolVersion::new(2, 0, 0))),
                Some(BuildProfile::CURRENT)
            ),
            Some(BlockReason::Protocol)
        );
    }

    #[test]
    fn attach_block_refuses_profile_mismatch_and_unknown() {
        assert_eq!(
            attach_block(Some(PROTOCOL_RANGE), Some(other_profile())),
            Some(BlockReason::ProfileMismatch)
        );
        assert_eq!(
            attach_block(Some(PROTOCOL_RANGE), None),
            Some(BlockReason::ProfileUnknown)
        );
        assert_eq!(attach_block(None, None), Some(BlockReason::ProtocolUnknown));
    }

    #[test]
    fn attach_block_protocol_takes_precedence_over_profile() {
        assert_eq!(
            attach_block(
                Some(ProtocolRange::exact(ProtocolVersion::new(2, 0, 0))),
                None
            ),
            Some(BlockReason::Protocol)
        );
    }
}
