//! The data plane's error vocabulary: why a request failed ([`ErrorCode`], in
//! `ServerMessage::Error`) and why a handshake was refused ([`AuthFailure`], in
//! `ServerMessage::AuthResult`).

use std::fmt;

use serde::{Deserialize, Serialize};

use super::types::ProtocolRange;
use super::wire_enum::wire_enum;

/// Why a request failed, carried by `ServerMessage::Error`.
///
/// Decodes an unknown code as [`ErrorCode::Unknown`], so a newer daemon's code
/// does not make an older client drop the error with the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(remote = "Self")]
pub enum ErrorCode {
    /// The session (or a tab of it) named by the request does not exist.
    SessionNotFound,
    /// A session with that name already exists.
    SessionAlreadyExists,
    /// A request other than `Auth` / `AuthProof` arrived before the handshake
    /// finished, or an `AuthProof` arrived with no challenge behind it.
    NotAuthenticated,
    /// The frame did not decode as a `ClientMessage`.
    InvalidMessage,
    /// The daemon could not carry the request out.
    InternalError,
    /// Another client holds the pane's input lock.
    InputLocked,
    /// The daemon already runs as many sessions as it allows.
    SessionLimitReached,
    /// The pane named by the request does not exist.
    PaneNotFound,
    /// The client connection named by the request is not attached (issue #146).
    ClientNotFound,
    /// A code this build does not know, from a newer daemon.
    /// Sent only to relay a value received as `Unknown`.
    #[serde(other)]
    Unknown,
}
wire_enum!(ErrorCode);

/// Why the daemon refused a handshake, carried by a failed
/// `ServerMessage::AuthResult` next to its human-readable `reason`.
///
/// A client decides on this, never on the text: whether to retry, and what to
/// tell the user ([`AuthFailure::hint`]). Decodes an unknown refusal as
/// [`AuthFailure::Unknown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(remote = "Self")]
pub enum AuthFailure {
    /// The client's protocol range and the daemon's do not overlap. Checked
    /// first, before the token.
    ProtocolMismatch {
        /// The range the client offered in `Auth`.
        client: ProtocolRange,
        /// The range the daemon speaks.
        daemon: ProtocolRange,
    },
    /// The token is not this daemon run's token. A daemon issues a new token
    /// each time it starts, so a token from before a restart is refused.
    BadToken,
    /// The signature in `AuthProof` does not verify against the public key
    /// presented in `Auth`.
    IdentityRejected,
    /// A refusal this build does not know, from a newer daemon.
    /// Sent only to relay a value received as `Unknown`.
    #[serde(other)]
    Unknown,
}
wire_enum!(AuthFailure);

impl AuthFailure {
    /// What the user can do about this refusal, if anything.
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            Self::ProtocolMismatch { .. } => {
                Some("Hint: update kmux and kmuxd until their supported protocol ranges overlap.")
            }
            Self::BadToken | Self::IdentityRejected | Self::Unknown => None,
        }
    }
}

/// What a refused `AuthResult` says, for a person: the daemon's own `reason`,
/// else the typed failure's words, else (a daemon that sent neither) a bare
/// "rejected".
pub fn refusal_reason(reason: Option<String>, failure: Option<AuthFailure>) -> String {
    reason
        .or_else(|| failure.map(|f| f.to_string()))
        .unwrap_or_else(|| "rejected".to_string())
}

impl fmt::Display for AuthFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProtocolMismatch { client, daemon } => {
                write!(
                    f,
                    "protocol version mismatch: client={client}, daemon={daemon}"
                )
            }
            Self::BadToken => f.write_str("invalid token"),
            Self::IdentityRejected => f.write_str("identity verification failed"),
            Self::Unknown => f.write_str("refused for a reason this build does not know"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::{PROTOCOL_RANGE, ProtocolVersion};
    use super::*;

    fn sample_mismatch() -> AuthFailure {
        AuthFailure::ProtocolMismatch {
            client: ProtocolRange::exact(ProtocolVersion::new(2, 0, 0)),
            daemon: PROTOCOL_RANGE,
        }
    }

    #[test]
    fn every_auth_failure_roundtrips() {
        for failure in [
            sample_mismatch(),
            AuthFailure::BadToken,
            AuthFailure::IdentityRejected,
        ] {
            let bytes = rmp_serde::to_vec_named(&failure).expect("encode");
            let back: AuthFailure = rmp_serde::from_slice(&bytes).expect("decode");
            assert_eq!(back, failure);
        }
    }

    #[test]
    fn only_a_protocol_mismatch_carries_the_upgrade_hint() {
        assert!(sample_mismatch().hint().unwrap().contains("ranges overlap"));
        for failure in [
            AuthFailure::BadToken,
            AuthFailure::IdentityRejected,
            AuthFailure::Unknown,
        ] {
            assert_eq!(failure.hint(), None, "{failure:?}");
        }
    }

    #[test]
    fn auth_failure_text_names_the_refusal() {
        assert_eq!(
            sample_mismatch().to_string(),
            format!("protocol version mismatch: client=2.0.0, daemon={PROTOCOL_RANGE}")
        );
        assert_eq!(AuthFailure::BadToken.to_string(), "invalid token");
        assert_eq!(
            AuthFailure::IdentityRejected.to_string(),
            "identity verification failed"
        );
        assert!(AuthFailure::Unknown.to_string().contains("does not know"));
    }

    #[test]
    fn refusal_reason_prefers_the_daemons_words() {
        assert_eq!(
            refusal_reason(Some("go away".into()), Some(AuthFailure::BadToken)),
            "go away"
        );
        assert_eq!(
            refusal_reason(None, Some(AuthFailure::BadToken)),
            "invalid token"
        );
        assert_eq!(refusal_reason(None, None), "rejected");
    }

    /// A newer daemon's refusal decodes as `Unknown`, whatever its shape.
    #[test]
    fn an_unknown_auth_failure_decodes_to_unknown() {
        #[derive(Serialize)]
        enum Newer {
            RateLimited { retry_after_secs: u32 },
        }
        let bytes = rmp_serde::to_vec_named(&Newer::RateLimited {
            retry_after_secs: 3,
        })
        .expect("encode");
        let back: AuthFailure = rmp_serde::from_slice(&bytes).expect("decode");
        assert_eq!(back, AuthFailure::Unknown);
    }

    /// A newer daemon's error code decodes as `Unknown`.
    #[test]
    fn an_unknown_error_code_decodes_to_unknown() {
        let bytes = rmp_serde::to_vec_named("TabNotFound").expect("encode");
        let back: ErrorCode = rmp_serde::from_slice(&bytes).expect("decode");
        assert_eq!(back, ErrorCode::Unknown);
        let known = rmp_serde::to_vec_named(&ErrorCode::PaneNotFound).expect("encode");
        assert_eq!(
            known,
            rmp_serde::to_vec_named("PaneNotFound").expect("encode")
        );
    }
}
