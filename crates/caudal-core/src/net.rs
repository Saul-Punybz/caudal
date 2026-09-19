//! IP address helpers shared by every protocol: CIDR parsing/matching (used
//! by `[server] trusted_proxies` and by `caudal-access`'s `[[access]]`
//! rules) and resolving the real client address behind a trusted reverse
//! proxy.

use std::net::IpAddr;

/// An IPv4 or IPv6 network: an address plus a prefix length. A bare address
/// (no `/n`) is a host route (`/32` or `/128`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cidr {
    addr: IpAddr,
    prefix: u8,
}

impl Cidr {
    /// Parses `"1.2.3.0/24"`, `"::1/128"`, or a bare address (host route).
    pub fn parse(s: &str) -> Result<Self, String> {
        let (addr_part, prefix_part) = match s.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (s, None),
        };
        let addr: IpAddr = addr_part.trim().parse().map_err(|_| format!("`{s}` is not a valid IP or CIDR"))?;
        let max_prefix = match addr {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        };
        let prefix = match prefix_part {
            Some(p) => {
                let p: u8 = p.trim().parse().map_err(|_| format!("`{s}` has a non-numeric prefix length"))?;
                if p > max_prefix {
                    return Err(format!("`{s}` prefix length exceeds {max_prefix}"));
                }
                p
            }
            None => max_prefix,
        };
        Ok(Self { addr, prefix })
    }

    /// Whether `ip` falls inside this network. A v4 network never contains a
    /// v6 address and vice versa (no mapped-address coercion).
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = v4_mask(self.prefix);
                (u32::from(net) & mask) == (u32::from(ip) & mask)
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = v6_mask(self.prefix);
                (u128::from(net) & mask) == (u128::from(ip) & mask)
            }
            _ => false,
        }
    }
}

impl std::fmt::Display for Cidr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

fn v4_mask(prefix: u8) -> u32 {
    if prefix == 0 { 0 } else { u32::MAX << (32 - prefix as u32) }
}

fn v6_mask(prefix: u8) -> u128 {
    if prefix == 0 { 0 } else { u128::MAX << (128 - prefix as u32) }
}

/// Resolves the address a request should be judged by: `peer` (the TCP/UDP
/// peer address) unless `peer` is itself a trusted reverse proxy, in which
/// case the right-most `X-Forwarded-For` entry that is *not* itself a
/// trusted proxy is used (walking right to left skips proxy hops closer to
/// us). An untrusted `peer` never has its header honored, so a client can't
/// spoof its own address by sending the header directly.
pub fn resolve_forwarded(peer: IpAddr, forwarded_for: Option<&str>, trusted: &[Cidr]) -> IpAddr {
    if !trusted.iter().any(|c| c.contains(peer)) {
        return peer;
    }
    let Some(xff) = forwarded_for else { return peer };
    for part in xff.split(',').rev() {
        if let Ok(ip) = part.trim().parse::<IpAddr>()
            && !trusted.iter().any(|c| c.contains(ip))
        {
            return ip;
        }
    }
    peer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_address_as_host_route() {
        let c = Cidr::parse("10.0.0.5").unwrap();
        assert!(c.contains("10.0.0.5".parse().unwrap()));
        assert!(!c.contains("10.0.0.6".parse().unwrap()));
    }

    #[test]
    fn parses_v4_cidr() {
        let c = Cidr::parse("10.0.0.0/8").unwrap();
        assert!(c.contains("10.1.2.3".parse().unwrap()));
        assert!(!c.contains("11.0.0.0".parse().unwrap()));
    }

    #[test]
    fn parses_v6_cidr() {
        let c = Cidr::parse("2001:db8::/32").unwrap();
        assert!(c.contains("2001:db8::1".parse().unwrap()));
        assert!(!c.contains("2001:db9::1".parse().unwrap()));
    }

    #[test]
    fn v4_and_v6_never_cross_match() {
        let c = Cidr::parse("0.0.0.0/0").unwrap();
        assert!(!c.contains("::1".parse().unwrap()));
    }

    #[test]
    fn zero_prefix_matches_everything() {
        assert!(Cidr::parse("0.0.0.0/0").unwrap().contains("255.255.255.255".parse().unwrap()));
        assert!(Cidr::parse("::/0").unwrap().contains("ffff::1".parse().unwrap()));
    }

    #[test]
    fn rejects_garbage() {
        assert!(Cidr::parse("not-an-ip").is_err());
        assert!(Cidr::parse("10.0.0.0/33").is_err());
        assert!(Cidr::parse("10.0.0.0/abc").is_err());
    }

    #[test]
    fn untrusted_peer_ignores_the_header() {
        let peer = "203.0.113.9".parse().unwrap();
        let resolved = resolve_forwarded(peer, Some("198.51.100.1"), &[]);
        assert_eq!(resolved, peer);
    }

    #[test]
    fn trusted_peer_uses_rightmost_untrusted_hop() {
        let peer = "10.0.0.1".parse().unwrap();
        let trusted = [Cidr::parse("10.0.0.0/8").unwrap()];
        // client -> proxy1 (untrusted-ish but still forwarded) -> proxy2(trusted, our peer)
        let resolved = resolve_forwarded(peer, Some("198.51.100.1, 10.0.0.2"), &trusted);
        assert_eq!(resolved, "198.51.100.1".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn trusted_peer_no_header_falls_back_to_peer() {
        let peer = "10.0.0.1".parse().unwrap();
        let trusted = [Cidr::parse("10.0.0.0/8").unwrap()];
        assert_eq!(resolve_forwarded(peer, None, &trusted), peer);
    }

    #[test]
    fn trusted_peer_all_hops_trusted_falls_back_to_peer() {
        let peer = "10.0.0.1".parse().unwrap();
        let trusted = [Cidr::parse("10.0.0.0/8").unwrap()];
        assert_eq!(resolve_forwarded(peer, Some("10.0.0.5, 10.0.0.2"), &trusted), peer);
    }
}
