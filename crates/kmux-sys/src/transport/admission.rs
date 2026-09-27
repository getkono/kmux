//! Which accepted connections may run a handshake (issue #207).
//!
//! A handshake is the part of a connection a peer controls the length of, so
//! the listeners bound how many run at once: [`MAX_PENDING_HANDSHAKES`] in all,
//! and [`MAX_PENDING_HANDSHAKES_PER_SOURCE`] from any one source. A connection
//! that finds either bound reached is refused at once rather than queued, so a
//! peer that opens connections and never finishes them fills its own share
//! and nobody else's. A QUIC client whose address is not yet validated is sent
//! a stateless Retry first once [`QUIC_RETRY_ABOVE`] handshakes are in flight,
//! so a spoofed source address can neither take a slot nor spend another
//! host's per-source share.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, PoisonError};

use thiserror::Error;

/// Most handshakes a listener runs at once, across every source. Each is a
/// task, a socket and TLS state; legitimate clients reconnect a handful at a
/// time, so this is far above what they produce.
pub const MAX_PENDING_HANDSHAKES: usize = 64;

/// Most handshakes a listener runs at once for one source: an IPv4 address,
/// or an IPv6 /64 (one host's usual allocation, so an attacker cannot fan out
/// across the addresses of its own prefix). A client reconnecting its
/// transports opens a few at once; this leaves it room while keeping any one
/// source to an eighth of [`MAX_PENDING_HANDSHAKES`].
pub const MAX_PENDING_HANDSHAKES_PER_SOURCE: usize = 8;

/// In-flight handshakes past which a QUIC client that has not proved its
/// address is sent a stateless Retry before it may take a slot. Below it a
/// QUIC connect costs one round trip less; above it, only a source that can
/// receive packets at its claimed address counts against that address's
/// share. Equal to the per-source cap, so a single source cannot reach its cap
/// unvalidated without also crossing this threshold.
pub const QUIC_RETRY_ABOVE: usize = MAX_PENDING_HANDSHAKES_PER_SOURCE;

/// The bounds a listener holds its handshakes to. Built by
/// [`HandshakeLimits::new`], which refuses a zero bound: a listener allowed no
/// handshakes at all could never serve anyone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandshakeLimits {
    total: NonZeroUsize,
    per_source: NonZeroUsize,
    retry_above: usize,
}

/// A [`HandshakeLimits`] that could not be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum HandshakeLimitsError {
    /// The listener-wide bound was zero.
    #[error("the handshake limit must allow at least one handshake")]
    ZeroTotal,
    /// The per-source bound was zero.
    #[error("the per-source handshake limit must allow at least one handshake")]
    ZeroPerSource,
}

impl HandshakeLimits {
    /// The daemon's limits: [`MAX_PENDING_HANDSHAKES`],
    /// [`MAX_PENDING_HANDSHAKES_PER_SOURCE`] and [`QUIC_RETRY_ABOVE`].
    pub const DAEMON: Self = Self {
        total: NonZeroUsize::new(MAX_PENDING_HANDSHAKES).unwrap(),
        per_source: NonZeroUsize::new(MAX_PENDING_HANDSHAKES_PER_SOURCE).unwrap(),
        retry_above: QUIC_RETRY_ABOVE,
    };

    /// At most `total` handshakes at once, `per_source` of them from one
    /// source, with an unvalidated QUIC client retried once `retry_above` are
    /// in flight (`0`: always).
    ///
    /// # Errors
    ///
    /// When `total` or `per_source` is zero.
    pub fn new(
        total: usize,
        per_source: usize,
        retry_above: usize,
    ) -> Result<Self, HandshakeLimitsError> {
        Ok(Self {
            total: NonZeroUsize::new(total).ok_or(HandshakeLimitsError::ZeroTotal)?,
            per_source: NonZeroUsize::new(per_source).ok_or(HandshakeLimitsError::ZeroPerSource)?,
            retry_above,
        })
    }
}

/// What to do with a connection that has just been accepted.
#[derive(Debug)]
pub(super) enum Admission {
    /// Run its handshake, holding this slot until it ends.
    Admit(Slot),
    /// Ask the (QUIC) client to prove its address first.
    Retry,
    /// Turn it away now.
    Refuse(Refusal),
}

/// Why a connection was turned away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Refusal {
    /// The listener-wide bound is reached.
    Full,
    /// This source's bound is reached.
    SourceFull,
}

/// The handshakes a listener has in flight, in all and per source.
#[derive(Debug, Default, Clone)]
pub(super) struct InFlight {
    counts: Arc<Mutex<Counts>>,
}

#[derive(Debug, Default)]
struct Counts {
    total: usize,
    by_source: HashMap<IpAddr, usize>,
}

impl InFlight {
    /// Decide on a connection from `source` (`None`: a local socket, held to
    /// the listener-wide bound only) that `needs_validation` (a QUIC client
    /// whose address is unproven).
    pub(super) fn admit(
        &self,
        limits: &HandshakeLimits,
        source: Option<IpAddr>,
        needs_validation: bool,
    ) -> Admission {
        let source = source.map(source_key);
        let mut counts = self.lock();
        if needs_validation && counts.total >= limits.retry_above {
            return Admission::Retry;
        }
        if counts.total >= limits.total.get() {
            return Admission::Refuse(Refusal::Full);
        }
        if let Some(ip) = source {
            let from_source = counts.by_source.entry(ip).or_default();
            if *from_source >= limits.per_source.get() {
                return Admission::Refuse(Refusal::SourceFull);
            }
            *from_source += 1;
        }
        counts.total += 1;
        Admission::Admit(Slot {
            in_flight: self.clone(),
            source,
        })
    }

    /// Handshakes in flight, in all.
    #[cfg(test)]
    fn total(&self) -> usize {
        self.lock().total
    }

    /// Handshakes in flight from `source`.
    #[cfg(test)]
    fn in_flight_from(&self, source: IpAddr) -> usize {
        self.lock()
            .by_source
            .get(&source_key(source))
            .copied()
            .unwrap_or_default()
    }

    fn release(&self, source: Option<IpAddr>) {
        let mut counts = self.lock();
        counts.total -= 1;
        if let Some(ip) = source
            && let Some(from_source) = counts.by_source.get_mut(&ip)
        {
            *from_source -= 1;
            if *from_source == 0 {
                counts.by_source.remove(&ip);
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Counts> {
        self.counts.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One admitted handshake's place in [`InFlight`], given back when dropped —
/// whether the handshake finished, failed or timed out.
#[derive(Debug)]
pub(super) struct Slot {
    in_flight: InFlight,
    source: Option<IpAddr>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.in_flight.release(self.source);
    }
}

/// The source a connection from `ip` counts against: the IPv4 address (an
/// IPv4-mapped IPv6 address is the IPv4 one), or the IPv6 /64.
fn source_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let prefix = u128::from(v6) & !u128::from(u64::MAX);
                IpAddr::V6(Ipv6Addr::from(prefix))
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn limits(total: usize, per_source: usize, retry_above: usize) -> HandshakeLimits {
        HandshakeLimits::new(total, per_source, retry_above).unwrap()
    }

    fn admitted(admission: Admission) -> Slot {
        match admission {
            Admission::Admit(slot) => slot,
            other => panic!("expected an admission, got {other:?}"),
        }
    }

    fn refused(admission: &Admission) -> Option<Refusal> {
        match admission {
            Admission::Refuse(why) => Some(*why),
            _ => None,
        }
    }

    #[test]
    fn a_zero_bound_is_refused_at_construction() {
        assert_eq!(
            HandshakeLimits::new(0, 1, 0),
            Err(HandshakeLimitsError::ZeroTotal)
        );
        assert_eq!(
            HandshakeLimits::new(1, 0, 0),
            Err(HandshakeLimitsError::ZeroPerSource)
        );
        let one = HandshakeLimits::new(1, 1, 0).unwrap();
        assert_eq!(
            (one.total.get(), one.per_source.get(), one.retry_above),
            (1, 1, 0)
        );
    }

    #[test]
    fn the_daemon_limits_are_the_named_constants() {
        assert_eq!(
            HandshakeLimits::DAEMON,
            limits(
                MAX_PENDING_HANDSHAKES,
                MAX_PENDING_HANDSHAKES_PER_SOURCE,
                QUIC_RETRY_ABOVE
            )
        );
        const {
            assert!(MAX_PENDING_HANDSHAKES_PER_SOURCE < MAX_PENDING_HANDSHAKES);
            assert!(QUIC_RETRY_ABOVE <= MAX_PENDING_HANDSHAKES_PER_SOURCE);
        }
    }

    /// One source fills its own share and is refused past it; another
    /// source is still admitted, and a slot given back is reusable.
    #[test]
    fn one_source_is_refused_past_its_share_and_others_are_not() {
        let in_flight = InFlight::default();
        let limits = limits(8, 2, 8);
        let noisy = ip("192.0.2.1");

        let first = admitted(in_flight.admit(&limits, Some(noisy), false));
        let _second = admitted(in_flight.admit(&limits, Some(noisy), false));
        assert_eq!(
            refused(&in_flight.admit(&limits, Some(noisy), false)),
            Some(Refusal::SourceFull)
        );
        assert_eq!(
            in_flight.in_flight_from(noisy),
            2,
            "a refusal takes nothing"
        );

        let _other = admitted(in_flight.admit(&limits, Some(ip("192.0.2.2")), false));
        assert_eq!(in_flight.total(), 3);

        drop(first);
        assert_eq!((in_flight.total(), in_flight.in_flight_from(noisy)), (2, 1));
        let _again = admitted(in_flight.admit(&limits, Some(noisy), false));
    }

    /// The listener-wide bound refuses everyone once reached, local sockets
    /// included, and frees up as handshakes end.
    #[test]
    fn the_listener_is_refused_past_its_bound() {
        let in_flight = InFlight::default();
        let limits = limits(2, 2, 2);
        let a = admitted(in_flight.admit(&limits, Some(ip("192.0.2.1")), false));
        let _b = admitted(in_flight.admit(&limits, None, false));
        assert_eq!(
            refused(&in_flight.admit(&limits, Some(ip("192.0.2.3")), false)),
            Some(Refusal::Full)
        );
        assert_eq!(
            refused(&in_flight.admit(&limits, None, false)),
            Some(Refusal::Full)
        );
        drop(a);
        assert_eq!(in_flight.total(), 1);
        assert_eq!(in_flight.in_flight_from(ip("192.0.2.1")), 0, "forgotten");
        let _c = admitted(in_flight.admit(&limits, None, false));
    }

    /// An unproven QUIC address is retried once the threshold is reached,
    /// before any bound is consulted and without taking a slot; a proven one,
    /// or any below the threshold, is judged by the bounds alone.
    #[test]
    fn an_unvalidated_client_is_retried_above_the_threshold() {
        let in_flight = InFlight::default();
        let limits = limits(4, 4, 1);
        let src = Some(ip("198.51.100.7"));

        let _first = admitted(in_flight.admit(&limits, src, true));
        assert!(matches!(
            in_flight.admit(&limits, src, true),
            Admission::Retry
        ));
        assert_eq!(in_flight.total(), 1, "a retry takes no slot");
        let _validated = admitted(in_flight.admit(&limits, src, false));
        assert_eq!(in_flight.total(), 2);
    }

    #[test]
    fn a_threshold_of_zero_retries_every_unvalidated_client() {
        let in_flight = InFlight::default();
        assert!(matches!(
            in_flight.admit(&limits(4, 4, 0), None, true),
            Admission::Retry
        ));
    }

    /// Addresses in one IPv6 /64 share a share; an IPv4-mapped address is its
    /// IPv4 address.
    #[test]
    fn sources_are_ipv4_addresses_and_ipv6_prefixes() {
        assert_eq!(source_key(ip("192.0.2.1")), ip("192.0.2.1"));
        assert_eq!(source_key(ip("::ffff:192.0.2.1")), ip("192.0.2.1"));
        assert_eq!(
            source_key(ip("2001:db8:1:2:aaaa:bbbb:cccc:dddd")),
            ip("2001:db8:1:2::")
        );
        assert_ne!(
            source_key(ip("2001:db8:1:2::1")),
            source_key(ip("2001:db8:1:3::1"))
        );

        let in_flight = InFlight::default();
        let limits = limits(8, 1, 8);
        let _one = admitted(in_flight.admit(&limits, Some(ip("2001:db8::1")), false));
        assert_eq!(
            refused(&in_flight.admit(&limits, Some(ip("2001:db8::2")), false)),
            Some(Refusal::SourceFull)
        );
    }
}
