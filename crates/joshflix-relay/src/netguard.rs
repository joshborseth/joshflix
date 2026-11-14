//! Network allowlist. The relay is never port-forwarded; this is a second
//! line of defense in case it ever is.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use axum::{
    extract::{ConnectInfo, Request},
    http::StatusCode,
    middleware::Next,
    response::Response,
};

use crate::api_error;

/// True when `ip` belongs to loopback, RFC 1918, the Tailscale CGNAT range
/// (100.64.0.0/10), IPv6 unique-local (fc00::/7, which includes Tailscale's
/// fd7a:115c:a1e0::/48), or link-local. IPv4-mapped IPv6 addresses are judged
/// by their IPv4 part.
pub fn is_allowed_peer(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => allowed_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => allowed_v4(v4),
            None => allowed_v6(v6),
        },
    }
}

fn allowed_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || (a == 100 && (64..=127).contains(&b))
}

fn allowed_v6(ip: Ipv6Addr) -> bool {
    let first = ip.segments()[0];
    ip.is_loopback()
        || (first & 0xfe00) == 0xfc00 // fc00::/7 unique local
        || (first & 0xffc0) == 0xfe80 // fe80::/10 link local
}

/// Normalizes IPv4-mapped IPv6 peers to plain IPv4 (dual-stack sockets).
pub fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

/// Middleware: 403 for peers outside the allowlist, and for requests that
/// carry no peer address at all (fail closed).
pub async fn guard(request: Request, next: Next) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip());
    match peer {
        Some(ip) if is_allowed_peer(ip) => next.run(request).await,
        Some(ip) => {
            tracing::warn!(%ip, path = request.uri().path(), "rejected peer outside allowlist");
            forbidden()
        }
        None => {
            tracing::error!("request without peer address; serve with connect info");
            forbidden()
        }
    }
}

fn forbidden() -> Response {
    api_error(
        StatusCode::FORBIDDEN,
        "forbidden",
        "This relay only answers on the home network and the tailnet.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed(s: &str) -> bool {
        is_allowed_peer(s.parse().expect("test ip"))
    }

    #[test]
    fn allows_private_ranges() {
        for ip in [
            "127.0.0.1",
            "127.8.9.10",
            "10.0.0.1",
            "10.255.255.255",
            "172.16.0.1",
            "172.31.255.254",
            "192.168.1.10",
            "100.64.0.1",
            "100.100.1.2",
            "100.127.255.255",
            "169.254.10.20",
            "::1",
            "fc00::1",
            "fd7a:115c:a1e0::1",
            "fe80::1",
            "febf::1",
            "::ffff:192.168.1.20",
            "::ffff:127.0.0.1",
        ] {
            assert!(allowed(ip), "{ip} should be allowed");
        }
    }

    #[test]
    fn rejects_public_and_odd_ranges() {
        for ip in [
            "8.8.8.8",
            "1.1.1.1",
            "172.15.255.255",
            "172.32.0.1",
            "192.169.0.1",
            "100.63.255.255",
            "100.128.0.0",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "::",
            "2001:4860:4860::8888",
            "fec0::1",
            "ff02::1",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
        ] {
            assert!(!allowed(ip), "{ip} should be rejected");
        }
    }

    #[test]
    fn canonicalizes_mapped_v4() {
        let mapped: IpAddr = "::ffff:10.1.2.3".parse().expect("ip");
        assert_eq!(
            canonical_ip(mapped),
            "10.1.2.3".parse::<IpAddr>().expect("ip")
        );
        let v6: IpAddr = "fd00::1".parse().expect("ip");
        assert_eq!(canonical_ip(v6), v6);
    }
}
