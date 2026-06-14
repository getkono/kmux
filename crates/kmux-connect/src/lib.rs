//! Connection & protocol-negotiation mechanism for kmux clients.
//!
//! Extracted from `kmux-client` (issue #121) so the same connect/negotiate
//! strategies can be reused by both a GUI client (local UDS to the daemon) and
//! `kmuxd` itself when it federates to a remote `kmuxd`. Nothing here holds
//! client *state* (session list, grids) — that lives in `kmux-client`.
//!
//! Contents: the bootstrap strategies + [`bootstrap::bootstrap_race`], the
//! one-shot [`pipeline::run_bootstrap`], transport setup (QUIC/TCP+TLS/UDS/SSH),
//! the [`supervisor::TransportSupervisor`], TOFU host pinning ([`hosts`]),
//! token handling ([`token`]), and local daemon lifecycle / control-socket
//! helpers ([`daemon`]).

pub mod bootstrap;
pub mod connect;
pub mod daemon;
pub mod hosts;
pub mod pipeline;
pub mod recovery;
pub mod ssh;
pub mod supervisor;
pub mod tcp_connect;
pub mod token;
