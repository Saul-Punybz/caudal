//! Server-side RTP/RTCP UDP port pool for unicast SETUP: an even RTP port
//! and its RTCP port right after it (`p`, `p+1`), both bound and free,
//! chosen from a configured range. Multicast is out of scope, so there is
//! no group address bookkeeping here at all.

use std::io;
use std::net::IpAddr;

use tokio::net::UdpSocket;
use tokio::sync::Mutex;

pub(crate) struct UdpPortPool {
    /// First even port in the usable range.
    start: u16,
    /// Number of RTP/RTCP pairs available starting at `start`.
    pairs: u16,
    /// Index (not port number) of the next pair to try, round-robin.
    cursor: Mutex<u16>,
}

impl UdpPortPool {
    pub(crate) fn new((start, end): (u16, u16)) -> Self {
        let start = if start % 2 == 0 { start } else { start.saturating_add(1) };
        let usable = end.saturating_sub(start);
        let pairs = (usable / 2).max(1);
        Self { start, pairs, cursor: Mutex::new(0) }
    }

    /// Binds an RTP/RTCP socket pair (`p` even, `p+1`) on `ip`, trying every
    /// pair in the configured range at most once before giving up.
    pub(crate) async fn allocate(&self, ip: IpAddr) -> io::Result<(UdpSocket, UdpSocket, u16)> {
        let mut cursor = self.cursor.lock().await;
        for _ in 0..self.pairs {
            let idx = *cursor;
            *cursor = (*cursor + 1) % self.pairs;
            let rtp_port = self.start + idx * 2;
            let rtcp_port = rtp_port + 1;
            let rtp_sock = match UdpSocket::bind((ip, rtp_port)).await {
                Ok(s) => s,
                Err(_) => continue,
            };
            let rtcp_sock = match UdpSocket::bind((ip, rtcp_port)).await {
                Ok(s) => s,
                Err(_) => continue,
            };
            return Ok((rtp_sock, rtcp_sock, rtp_port));
        }
        Err(io::Error::new(io::ErrorKind::AddrNotAvailable, "rtsp: no free UDP port pair in udp_port_range"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[tokio::test]
    async fn allocates_an_even_rtp_port_and_the_next_odd_rtcp_port() {
        let pool = UdpPortPool::new((20_000, 20_009));
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let (_rtp, _rtcp, rtp_port) = pool.allocate(ip).await.expect("allocate");
        assert_eq!(rtp_port % 2, 0);
        assert!((20_000..20_009).contains(&rtp_port));
    }

    #[tokio::test]
    async fn does_not_hand_out_the_same_pair_twice_while_both_are_held() {
        let pool = UdpPortPool::new((20_100, 20_103)); // exactly one pair: 20100/20101
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let (rtp1, rtcp1, port1) = pool.allocate(ip).await.expect("first allocate");
        // The only pair is held; a second allocate must fail rather than
        // silently double-binding (which the OS would reject anyway).
        let err = pool.allocate(ip).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AddrNotAvailable);
        drop((rtp1, rtcp1));
        // Freed: the same pair becomes available again.
        let (_rtp2, _rtcp2, port2) = pool.allocate(ip).await.expect("third allocate after freeing");
        assert_eq!(port1, port2);
    }
}
