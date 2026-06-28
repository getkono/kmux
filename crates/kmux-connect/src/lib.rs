//! Connection & protocol-negotiation mechanism for kmux clients.
//!
//! Extracted from `kmux-client` (issue #121) so the same connect/negotiate
//! strategies can be reused by both a GUI client (local UDS to the daemon) and
//! `kmuxd` itself when it federates to a remote `kmuxd`. Nothing here holds
//! client *state* (session list, grids) — that lives in `kmux-client`.
//!
//! Contents: the one-shot [`pipeline::run_bootstrap`], transport setup
//! (QUIC/TCP+TLS/UDS/SSH),
//! the [`supervisor::TransportSupervisor`], TOFU host pinning ([`hosts`]),
//! token handling ([`token`]), and local daemon lifecycle / control-socket
//! helpers ([`daemon`]).

pub mod connect;
pub mod daemon;
pub mod hosts;
pub mod pipeline;
pub mod ssh;
pub mod supervisor;
pub mod tcp_connect;
pub mod token;

use std::sync::OnceLock;

use kmux_protocol::messages::FrontendKind;

/// Process-wide frontend identity, reported in every `Auth` frame this process
/// sends (issue: client↔daemon build skew). It is a per-binary constant, so a
/// `OnceLock` set once at startup is the right model — threading it through every
/// connect/bootstrap signature would be noise.
static FRONTEND_KIND: OnceLock<FrontendKind> = OnceLock::new();

/// Record which frontend this process is. Called once at GUI startup
/// (`kmux-gtk` → [`FrontendKind::Gtk`], `kmux-ffi`/Swift → [`FrontendKind::Swift`]);
/// the toolkit-free CLI leaves it at the default [`FrontendKind::Cli`].
pub fn set_frontend_kind(kind: FrontendKind) {
    let _ = FRONTEND_KIND.set(kind);
}

/// The frontend identity set by [`set_frontend_kind`], or [`FrontendKind::Cli`]
/// when unset (a plain `kmux` CLI invocation).
pub(crate) fn frontend_kind() -> FrontendKind {
    FRONTEND_KIND.get().copied().unwrap_or_default()
}
