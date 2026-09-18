//! Which addresses to advertise as ICE host candidates, and which of them a
//! datagram arrived on.
//!
//! All peers share one UDP socket. str0m only answers STUN that arrived on
//! one of its local candidates, so each datagram must be labelled with the
//! candidate address it was sent to. With a specific bind address that is
//! the bind address. With a wildcard bind (`0.0.0.0`) the socket cannot tell
//! (that needs `IP_PKTINFO`), so we ask the kernel which local address it
//! would use to reach the sender (a connected, never-used UDP socket; no
//! packet is sent) — replies leave from that address too, so it is the only
//! candidate that can succeed for that sender anyway.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, UdpSocket};

/// Host candidates for a socket bound to `local`.
///
/// - `public_ips` non-empty: exactly those, with the socket's port (1:1 NAT).
/// - Specific bind address: that address.
/// - Wildcard bind: every up interface address of the socket's family from
///   `getifaddrs` (via `if-addrs`), skipping loopback and link-local unless
///   nothing else exists.
pub(crate) fn candidate_addrs(local: SocketAddr, public_ips: &[IpAddr]) -> Vec<SocketAddr> {
    let port = local.port();
    if !public_ips.is_empty() {
        return public_ips.iter().map(|ip| SocketAddr::new(*ip, port)).collect();
    }
    if !local.ip().is_unspecified() {
        return vec![local];
    }
    let v4 = local.is_ipv4();
    let mut routable = Vec::new();
    let mut loopback = Vec::new();
    match if_addrs::get_if_addrs() {
        Ok(ifs) => {
            for i in ifs {
                let ip = i.ip();
                if ip.is_ipv4() != v4 || ip.is_unspecified() || is_link_local(ip) {
                    continue;
                }
                let addr = SocketAddr::new(ip, port);
                if ip.is_loopback() {
                    loopback.push(addr);
                } else if !routable.contains(&addr) {
                    routable.push(addr);
                }
            }
        }
        Err(e) => tracing::warn!(error = %e, "webrtc: cannot list network interfaces"),
    }
    if routable.is_empty() { loopback } else { routable }
}

fn is_link_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v.is_link_local(),
        IpAddr::V6(v) => (v.segments()[0] & 0xffc0) == 0xfe80,
    }
}

/// Maps a sender to the local candidate its datagrams count as arriving on.
pub(crate) struct Destinations {
    candidates: Vec<SocketAddr>,
    fixed: bool,
    cache: HashMap<IpAddr, SocketAddr>,
}

impl Destinations {
    pub(crate) fn new(local: SocketAddr, candidates: Vec<SocketAddr>) -> Self {
        let fixed = !local.ip().is_unspecified() || candidates.len() <= 1;
        Self { candidates, fixed, cache: HashMap::new() }
    }

    pub(crate) fn for_source(&mut self, source: SocketAddr) -> Option<SocketAddr> {
        let same_family = |c: &&SocketAddr| c.is_ipv4() == source.is_ipv4();
        if self.fixed {
            return self.candidates.iter().find(same_family).or(self.candidates.first()).copied();
        }
        if let Some(d) = self.cache.get(&source.ip()) {
            return Some(*d);
        }
        let routed = route_to(source).and_then(|ip| self.candidates.iter().find(|c| c.ip() == ip).copied());
        let d = routed.or_else(|| self.candidates.iter().find(same_family).copied())?;
        if self.cache.len() > 4096 {
            self.cache.clear();
        }
        self.cache.insert(source.ip(), d);
        Some(d)
    }
}

/// The local address the kernel would send from to reach `to`.
fn route_to(to: SocketAddr) -> Option<IpAddr> {
    let any: SocketAddr = if to.is_ipv4() { ([0, 0, 0, 0], 0).into() } else { ([0u16; 8], 0).into() };
    let s = UdpSocket::bind(any).ok()?;
    s.connect(to).ok()?;
    Some(s.local_addr().ok()?.ip())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specific_bind_is_its_own_candidate() {
        let local: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        assert_eq!(candidate_addrs(local, &[]), vec![local]);
        let mut d = Destinations::new(local, vec![local]);
        assert_eq!(d.for_source("127.0.0.1:9".parse().unwrap()), Some(local));
    }

    #[test]
    fn public_ips_win() {
        let local: SocketAddr = "0.0.0.0:5000".parse().unwrap();
        let c = candidate_addrs(local, &["203.0.113.7".parse().unwrap()]);
        assert_eq!(c, vec!["203.0.113.7:5000".parse().unwrap()]);
    }

    #[test]
    fn wildcard_enumerates_interfaces() {
        let local: SocketAddr = "0.0.0.0:5000".parse().unwrap();
        let c = candidate_addrs(local, &[]);
        assert!(!c.is_empty());
        assert!(c.iter().all(|a| a.is_ipv4() && a.port() == 5000 && !a.ip().is_unspecified()));
        let mut d = Destinations::new(local, c.clone());
        let dest = d.for_source("127.0.0.1:9".parse().unwrap()).unwrap();
        assert!(c.contains(&dest));
    }
}
