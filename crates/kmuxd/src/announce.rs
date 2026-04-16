//! Audience-aware endpoint announcement.
//!
//! Phase 7: builds the `Vec<EndpointAdvert>` that `StatusResponse`/
//! `probe-or-start` return to clients. The server is the authority on which
//! transports it supports; the client only uses endpoints from this list.
//!
//! Audience filtering determines which endpoints are visible depending on how
//! the client connected:
//! - `Any`     — always included.
//! - `Local`   — only when the client came from the UDS control socket or loopback.
//! - `Lan`     — only when the peer is RFC-1918 / link-local.
//! - `SshOnly` — only inside SSH `probe-or-start` responses.

use std::net::SocketAddr;

use kmux_protocol::messages::TransportKind;
use kmux_protocol::transport::bootstrap::EndpointAdvert;

use crate::config::{Audience, ListenConfig, ListenKind};

/// How the client reached the server — determines which `Audience` tags pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapPath {
    /// Direct UDS control-socket query.
    Uds,
    /// SSH `probe-or-start` invocation.
    ///
    /// Used to include `SshOnly` and `Any` endpoints while excluding `Local`.
    /// Wired into the `probe-or-start` JSON response in a future phase.
    Ssh,
    /// Direct network connection (IP address known).
    ///
    /// Used to apply RFC-1918 `Lan` audience filtering for remote clients.
    /// Wired into the `AuthResult` endpoint advertisement in a future phase.
    /// Not yet used outside tests because `AuthResult` does not yet carry endpoint adverts.
    #[allow(dead_code)]
    Network { peer: SocketAddr },
}

/// Build an endpoint advert for a single enabled listener.
///
/// Returns `None` when the listener is disabled or its audience does not match
/// the bootstrap path.
pub fn advert_for(
    listener: &ListenConfig,
    path: BootstrapPath,
    public_host: Option<&str>,
) -> Option<EndpointAdvert> {
    if !listener.enabled {
        return None;
    }

    // Apply audience filter.
    let passes = match (listener.audience, path) {
        (Audience::Any, _) => true,
        (Audience::Local, BootstrapPath::Uds) => true,
        (Audience::Local, BootstrapPath::Network { peer }) => peer.ip().is_loopback(),
        (Audience::Local, _) => false,
        (Audience::SshOnly, BootstrapPath::Ssh) => true,
        (Audience::SshOnly, _) => false,
        (Audience::Lan, BootstrapPath::Network { peer }) => is_lan_address(peer),
        (Audience::Lan, _) => false,
    };

    if !passes {
        return None;
    }

    let address = match listener.kind {
        ListenKind::Unix => {
            // UDS address is the path (caller must resolve "auto" before calling).
            listener.path.clone()
        }
        ListenKind::Quic | ListenKind::TcpTls => {
            // Use public_host override when available; fall back to bind address.
            let host = public_host.unwrap_or(&listener.bind);
            format!("{host}:{}", listener.port)
        }
    };

    let kind = match listener.kind {
        ListenKind::Quic => TransportKind::Quic,
        ListenKind::TcpTls => TransportKind::TcpTls,
        ListenKind::Unix => TransportKind::Uds,
    };

    Some(EndpointAdvert { kind, address })
}

/// Build the full endpoint list for a given bootstrap path.
pub fn build_endpoint_list(
    listeners: &[ListenConfig],
    path: BootstrapPath,
    public_host: Option<&str>,
) -> Vec<EndpointAdvert> {
    listeners
        .iter()
        .filter_map(|l| advert_for(l, path, public_host))
        .collect()
}

/// Returns `true` if `addr` is an RFC-1918 / link-local / loopback address.
fn is_lan_address(addr: SocketAddr) -> bool {
    let ip = addr.ip();
    if ip.is_loopback() {
        return true;
    }
    match ip {
        std::net::IpAddr::V4(v4) => {
            // 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 169.254.0.0/16
            let o = v4.octets();
            o[0] == 10
                || (o[0] == 172 && (o[1] >= 16 && o[1] <= 31))
                || (o[0] == 192 && o[1] == 168)
                || (o[0] == 169 && o[1] == 254)
        }
        std::net::IpAddr::V6(v6) => {
            // fc00::/7 (ULA) or fe80::/10 (link-local)
            let s = v6.segments();
            (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;
    use crate::config::ListenConfig;

    fn quic_listener(audience: Audience) -> ListenConfig {
        ListenConfig {
            kind: ListenKind::Quic,
            bind: "0.0.0.0".into(),
            port: 8443,
            enabled: true,
            path: "auto".into(),
            audience,
            priority: 0,
        }
    }

    fn tcp_listener(audience: Audience) -> ListenConfig {
        ListenConfig {
            kind: ListenKind::TcpTls,
            bind: "127.0.0.1".into(),
            port: 8444,
            enabled: true,
            path: "auto".into(),
            audience,
            priority: 0,
        }
    }

    fn uds_listener(audience: Audience) -> ListenConfig {
        ListenConfig {
            kind: ListenKind::Unix,
            bind: "::".into(),
            port: 0,
            enabled: true,
            path: "/run/user/1000/kmux/daemon-data.sock".into(),
            audience,
            priority: 0,
        }
    }

    // ── audience_filtering_local_ssh_public ───────────────────────────────────

    #[test]
    fn audience_any_visible_to_all_paths() {
        let l = quic_listener(Audience::Any);
        assert!(advert_for(&l, BootstrapPath::Uds, None).is_some());
        assert!(advert_for(&l, BootstrapPath::Ssh, None).is_some());
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), 0);
        assert!(advert_for(&l, BootstrapPath::Network { peer }, None).is_some());
    }

    #[test]
    fn audience_local_visible_only_to_uds_and_loopback() {
        let l = uds_listener(Audience::Local);
        assert!(advert_for(&l, BootstrapPath::Uds, None).is_some());

        let loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        assert!(advert_for(&l, BootstrapPath::Network { peer: loopback }, None).is_some());

        let public = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), 0);
        assert!(advert_for(&l, BootstrapPath::Network { peer: public }, None).is_none());
        assert!(advert_for(&l, BootstrapPath::Ssh, None).is_none());
    }

    #[test]
    fn audience_ssh_only_visible_only_via_ssh() {
        let l = tcp_listener(Audience::SshOnly);
        assert!(advert_for(&l, BootstrapPath::Ssh, None).is_some());
        assert!(advert_for(&l, BootstrapPath::Uds, None).is_none());
        let public = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 0);
        assert!(advert_for(&l, BootstrapPath::Network { peer: public }, None).is_none());
    }

    #[test]
    fn audience_lan_visible_to_rfc1918() {
        let l = quic_listener(Audience::Lan);
        let lan = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 0);
        assert!(advert_for(&l, BootstrapPath::Network { peer: lan }, None).is_some());

        let public = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), 0);
        assert!(advert_for(&l, BootstrapPath::Network { peer: public }, None).is_none());
    }

    #[test]
    fn disabled_listener_not_announced() {
        let mut l = quic_listener(Audience::Any);
        l.enabled = false;
        assert!(advert_for(&l, BootstrapPath::Uds, None).is_none());
    }

    #[test]
    fn public_host_substituted_for_quic() {
        let l = quic_listener(Audience::Any);
        let advert = advert_for(&l, BootstrapPath::Ssh, Some("prod.example.com")).unwrap();
        assert_eq!(advert.address, "prod.example.com:8443");
    }

    #[test]
    fn build_endpoint_list_ssh_path_excludes_local() {
        let listeners = vec![
            quic_listener(Audience::Any),
            tcp_listener(Audience::SshOnly),
            uds_listener(Audience::Local),
        ];
        let adverts = build_endpoint_list(&listeners, BootstrapPath::Ssh, None);
        // QUIC (any) + TCP+TLS (ssh-only); UDS (local) excluded for SSH.
        assert_eq!(adverts.len(), 2);
        assert!(adverts.iter().any(|a| a.kind == TransportKind::Quic));
        assert!(adverts.iter().any(|a| a.kind == TransportKind::TcpTls));
    }

    #[test]
    fn build_endpoint_list_uds_path_includes_local() {
        let listeners = vec![quic_listener(Audience::Any), uds_listener(Audience::Local)];
        let adverts = build_endpoint_list(&listeners, BootstrapPath::Uds, None);
        assert_eq!(adverts.len(), 2);
    }

    // ── is_lan_address ────────────────────────────────────────────────────────

    #[test]
    fn rfc1918_is_lan() {
        for ip in [
            "10.0.0.1",
            "10.255.255.255",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.0.1",
        ] {
            let addr: IpAddr = ip.parse().unwrap();
            assert!(
                is_lan_address(SocketAddr::new(addr, 0)),
                "{ip} should be LAN"
            );
        }
    }

    #[test]
    fn public_ip_is_not_lan() {
        let addr: IpAddr = "8.8.8.8".parse().unwrap();
        assert!(!is_lan_address(SocketAddr::new(addr, 0)));
    }
}
