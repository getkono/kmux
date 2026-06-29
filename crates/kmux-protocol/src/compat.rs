//! Single source of truth for client↔daemon compatibility classification.
//!
//! Three call sites used to re-spell the same protocol/profile/build-fingerprint
//! comparisons: `ensure_compatible_daemon` (the attach gate), `kmux daemon
//! status`, and `kmux client status` (and now `kmux status`). This module owns
//! the *decision* — what counts as same / different / unknown, and what is
//! blocking — while each caller keeps formatting its own user-facing prose.
//!
//! The reference is always *this* binary: [`protocol_match`] compares against
//! [`PROTOCOL_VERSION`] and [`profile_match`] against [`BuildProfile::CURRENT`],
//! both linked into whatever binary calls this.

use crate::dirs::BuildProfile;
use crate::messages::PROTOCOL_VERSION;

/// Three-way comparison of a peer's reported field against ours.
///
/// `Unknown` means the peer omitted the field — the wire sentinel `0` for the
/// protocol version, `None` for the build profile, or an empty build
/// fingerprint (a daemon predating that field). An `Unknown` is never *by
/// itself* a wire error, though the attach gate treats an unknown profile as
/// unverifiable (see [`attach_block`]).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Match3 {
    Same,
    Differ,
    Unknown,
}

/// Compare a peer's `protocol_version` against ours. The wire sentinel `0` (an
/// old daemon that did not report one) is [`Match3::Unknown`].
pub fn protocol_match(theirs: u32) -> Match3 {
    if theirs == 0 {
        Match3::Unknown
    } else if theirs == PROTOCOL_VERSION {
        Match3::Same
    } else {
        Match3::Differ
    }
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
    /// The protocol versions differ — the two cannot speak the wire format.
    Protocol,
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
/// Blocking: a *differing* protocol version, or a profile that differs *or* is
/// unknown. Protocol takes precedence over profile (its message is the more
/// actionable one). A protocol `Unknown` (sentinel `0`) and any
/// build-fingerprint skew are never blocking on their own — the first because an
/// old daemon predates version reporting, the second because a fingerprint gap
/// is advisory, not a wire incompatibility.
pub fn attach_block(protocol: u32, profile: Option<BuildProfile>) -> Option<BlockReason> {
    if protocol_match(protocol) == Match3::Differ {
        return Some(BlockReason::Protocol);
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
        assert_eq!(protocol_match(0), Match3::Unknown);
        assert_eq!(protocol_match(PROTOCOL_VERSION), Match3::Same);
        assert_eq!(
            protocol_match(PROTOCOL_VERSION.wrapping_add(1)),
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
            attach_block(PROTOCOL_VERSION, Some(BuildProfile::CURRENT)),
            None
        );
    }

    #[test]
    fn attach_block_tolerates_unknown_protocol_with_matching_profile() {
        // Sentinel protocol 0 (old daemon) is not blocking on its own.
        assert_eq!(attach_block(0, Some(BuildProfile::CURRENT)), None);
    }

    #[test]
    fn attach_block_refuses_protocol_mismatch() {
        assert_eq!(
            attach_block(
                PROTOCOL_VERSION.wrapping_add(1),
                Some(BuildProfile::CURRENT)
            ),
            Some(BlockReason::Protocol)
        );
    }

    #[test]
    fn attach_block_refuses_profile_mismatch_and_unknown() {
        assert_eq!(
            attach_block(PROTOCOL_VERSION, Some(other_profile())),
            Some(BlockReason::ProfileMismatch)
        );
        assert_eq!(
            attach_block(PROTOCOL_VERSION, None),
            Some(BlockReason::ProfileUnknown)
        );
        // Protocol unknown still lets an unknown profile block.
        assert_eq!(attach_block(0, None), Some(BlockReason::ProfileUnknown));
    }

    #[test]
    fn attach_block_protocol_takes_precedence_over_profile() {
        assert_eq!(
            attach_block(PROTOCOL_VERSION.wrapping_add(1), None),
            Some(BlockReason::Protocol)
        );
    }
}
