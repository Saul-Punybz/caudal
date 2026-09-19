//! The sending socket: a plain UDP socket with the multicast options set
//! through `socket2` (std and tokio expose TTL and loop, but not the
//! outgoing interface).

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

use socket2::{Domain, Protocol, Socket, Type};

use crate::{Interface, MulticastTarget};

/// Headroom for a keyframe's worth of datagrams handed to the kernel
/// between two pacer wakeups. Best effort: the OS may cap it lower.
const SEND_BUFFER: usize = 1 << 20;

pub(crate) fn open(target: &MulticastTarget) -> io::Result<tokio::net::UdpSocket> {
    let socket = match target.group {
        SocketAddr::V4(_) => {
            let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
            s.set_multicast_ttl_v4(target.ttl)?;
            s.set_multicast_loop_v4(target.loopback)?;
            let source = match target.interface {
                Interface::V4(addr) => {
                    s.set_multicast_if_v4(&addr)?;
                    addr
                }
                _ => Ipv4Addr::UNSPECIFIED,
            };
            // Bound to the interface's address, datagrams also carry it as
            // their source (what receivers filtering by source, SSM, see).
            s.bind(&SocketAddr::from((source, 0)).into())?;
            s
        }
        SocketAddr::V6(_) => {
            let s = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
            s.set_only_v6(true)?;
            s.set_multicast_hops_v6(target.ttl)?;
            s.set_multicast_loop_v6(target.loopback)?;
            if let Interface::V6Index(index) = target.interface {
                s.set_multicast_if_v6(index)?;
            }
            s.bind(&SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0)).into())?;
            s
        }
    };
    if let Err(err) = socket.set_send_buffer_size(SEND_BUFFER) {
        tracing::debug!(%err, "multicast: could not raise the send buffer");
    }
    socket.set_nonblocking(true)?;
    tokio::net::UdpSocket::from_std(socket.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Format;

    fn target(group: &str, interface: Interface) -> MulticastTarget {
        MulticastTarget {
            stream: "s".into(),
            group: group.parse().unwrap(),
            format: Format::Ts,
            ttl: 7,
            interface,
            loopback: true,
            pacing: true,
        }
    }

    #[tokio::test]
    async fn sets_ttl_loop_and_interface() {
        let sock = open(&target("239.255.0.1:5000", Interface::V4(Ipv4Addr::LOCALHOST))).unwrap();
        let s = socket2::SockRef::from(&sock);
        assert_eq!(s.multicast_ttl_v4().unwrap(), 7);
        assert!(s.multicast_loop_v4().unwrap());
        assert_eq!(s.multicast_if_v4().unwrap(), Ipv4Addr::LOCALHOST);
        assert_eq!(sock.local_addr().unwrap().ip(), std::net::IpAddr::V4(Ipv4Addr::LOCALHOST));
    }

    #[tokio::test]
    async fn default_interface_binds_the_wildcard() {
        let sock = open(&target("239.255.0.1:5000", Interface::Default)).unwrap();
        assert!(sock.local_addr().unwrap().ip().is_unspecified());
    }
}
