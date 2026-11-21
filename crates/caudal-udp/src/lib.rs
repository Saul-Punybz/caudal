//! MPEG-TS over UDP into Caudal: `[[udp.pull]]`.
//!
//! The way a lot of broadcast gear hands video around a building: an
//! encoder, a VLC or a playout box sends a transport stream to a UDP port,
//! unicast or to a multicast group, often wrapped in RTP (RFC 2250). Caudal
//! listens, joins the group when the address is multicast, and publishes
//! what arrives as `stream`.
//!
//! - `udp://239.1.1.1:5000`: join that group, take plain TS.
//! - `udp://0.0.0.0:5000` (or `udp://:5000`): unicast to this port.
//! - `rtp://...`: the same, each datagram carrying one RTP header first.
//! - `udp://[ff15::1]:5000`: IPv6 multicast.
//!
//! UDP has no connection: the stream is published on the first datagram
//! and ends after [`IDLE`] without one; the next datagram publishes it
//! again. A socket that cannot be opened (port in use, no such interface)
//! is retried every few seconds and logged, never panics the server.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use caudal_core::{BufferConfig, PublishError, Registry};
use caudal_ts::ingest::TsIngest;
use parking_lot::Mutex;

/// A stream that sends nothing for this long has stopped.
pub const IDLE: Duration = Duration::from_secs(5);
/// How long to wait before trying a socket that would not open again.
const RETRY: Duration = Duration::from_secs(5);
/// Asked of the kernel; it may grant less (Linux caps at `rmem_max`).
const RECV_BUFFER: usize = 4 << 20;

/// One `[[udp.pull]]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdpPull {
    pub stream: String,
    pub url: String,
    /// Local IPv4 address of the interface to join a multicast group on;
    /// `None`: the system's choice.
    pub interface: Option<Ipv4Addr>,
}

/// What a `udp://` / `rtp://` URL asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub addr: SocketAddr,
    pub rtp: bool,
}

impl Target {
    pub fn is_multicast(&self) -> bool {
        self.addr.ip().is_multicast()
    }
}

/// `udp://host:port` or `rtp://host:port`; an empty host is `0.0.0.0`.
pub fn parse_url(url: &str) -> Result<Target, String> {
    let (scheme, rest) = url.split_once("://").ok_or_else(|| format!("{url:?}: expected udp:// or rtp://"))?;
    let rtp = match scheme {
        "udp" => false,
        "rtp" => true,
        _ => return Err(format!("{url:?}: expected udp:// or rtp://")),
    };
    let rest = rest.split(['?', '/']).next().unwrap_or("");
    let rest = rest.strip_prefix('@').unwrap_or(rest); // VLC's `udp://@239.1.1.1:5000`
    let addr = if let Some(port) = rest.strip_prefix(':') {
        let port: u16 = port.parse().map_err(|_| format!("{url:?}: bad port"))?;
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, port))
    } else {
        rest.parse::<SocketAddr>().map_err(|_| format!("{url:?}: expected host:port (an IP address, not a name)"))?
    };
    if addr.port() == 0 {
        return Err(format!("{url:?}: needs a port"));
    }
    Ok(Target { addr, rtp })
}

/// The bytes of a datagram that are TS: all of it for plain UDP; after the
/// RTP header (12 bytes + CSRCs + extension, minus padding) for RTP.
/// `None` for an RTP packet that is not well formed.
pub fn ts_payload(datagram: &[u8], rtp: bool) -> Option<&[u8]> {
    if !rtp {
        return Some(datagram);
    }
    if datagram.len() < 12 || datagram[0] >> 6 != 2 {
        return None;
    }
    let csrc = usize::from(datagram[0] & 0x0f);
    let mut start = 12 + 4 * csrc;
    if datagram[0] & 0x10 != 0 {
        let ext = datagram.get(start + 2..start + 4)?;
        start += 4 + 4 * usize::from(u16::from_be_bytes([ext[0], ext[1]]));
    }
    let mut end = datagram.len();
    if datagram[0] & 0x20 != 0 {
        end = end.checked_sub(usize::from(*datagram.last()?))?;
    }
    datagram.get(start..end)
}

fn open(target: Target, interface: Option<Ipv4Addr>) -> std::io::Result<tokio::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let domain = if target.addr.is_ipv4() { Domain::IPV4 } else { Domain::IPV6 };
    let s = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    // Several receivers of one group on this machine (a monitor, a second
    // Caudal) must all be able to bind the port.
    s.set_reuse_address(true)?;
    #[cfg(unix)]
    s.set_reuse_port(true)?;
    let _ = s.set_recv_buffer_size(RECV_BUFFER);
    let bind = match target.addr.ip() {
        // Bound to the wildcard so the kernel hands us the group's traffic
        // on every platform; the join below picks what we get.
        IpAddr::V4(ip) if ip.is_multicast() => SocketAddr::from((Ipv4Addr::UNSPECIFIED, target.addr.port())),
        IpAddr::V6(ip) if ip.is_multicast() => {
            s.set_only_v6(true)?;
            SocketAddr::from((Ipv6Addr::UNSPECIFIED, target.addr.port()))
        }
        _ => target.addr,
    };
    s.bind(&bind.into())?;
    match target.addr.ip() {
        IpAddr::V4(g) if g.is_multicast() => s.join_multicast_v4(&g, &interface.unwrap_or(Ipv4Addr::UNSPECIFIED))?,
        IpAddr::V6(g) if g.is_multicast() => s.join_multicast_v6(&g, 0)?,
        _ => {}
    }
    s.set_nonblocking(true)?;
    tokio::net::UdpSocket::from_std(s.into())
}

/// Runs one pull until its task is aborted.
async fn run(pull: UdpPull, registry: Arc<Registry>, buffer: BufferConfig) {
    let target = match parse_url(&pull.url) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(stream = %pull.stream, error = %e, "udp pull: bad url; not started");
            return;
        }
    };
    let socket = loop {
        match open(target, pull.interface) {
            Ok(s) => break s,
            Err(e) => {
                tracing::warn!(stream = %pull.stream, url = %pull.url, error = %e, "udp pull: cannot open the socket; retrying");
                tokio::time::sleep(RETRY).await;
            }
        }
    };
    tracing::info!(
        stream = %pull.stream, url = %pull.url, multicast = target.is_multicast(), rtp = target.rtp,
        "udp pull listening"
    );
    let mut buf = vec![0u8; 65_536];
    let mut ingest: Option<TsIngest> = None;
    loop {
        let wait = if ingest.is_some() { IDLE } else { Duration::from_secs(3600) };
        match tokio::time::timeout(wait, socket.recv_from(&mut buf)).await {
            Ok(Ok((n, from))) => {
                let Some(ts) = ts_payload(&buf[..n], target.rtp) else { continue };
                if ingest.is_none() {
                    match registry.publish(&pull.stream, buffer) {
                        Ok(p) => {
                            tracing::info!(stream = %pull.stream, %from, "udp pull: data arriving; published");
                            ingest = Some(TsIngest::new(p));
                        }
                        Err(PublishError::Busy(_)) => continue, // someone else has the name; try on the next datagram
                        Err(PublishError::InvalidName) => {
                            tracing::error!(stream = %pull.stream, "udp pull: invalid stream name; stopped");
                            return;
                        }
                    }
                }
                if let Some(i) = ingest.as_mut() {
                    i.feed(ts);
                }
            }
            Ok(Err(e)) => {
                tracing::debug!(stream = %pull.stream, error = %e, "udp pull: recv error");
            }
            Err(_) => {
                if ingest.take().is_some() {
                    tracing::info!(stream = %pull.stream, idle = ?IDLE, "udp pull: no data; stream ended");
                }
            }
        }
    }
}

struct Entry {
    pull: UdpPull,
    task: tokio::task::JoinHandle<()>,
}

/// The running `[[udp.pull]]`s. Cheap to clone.
#[derive(Clone)]
pub struct UdpPullHandle {
    entries: Arc<Mutex<Vec<Entry>>>,
    buffer: BufferConfig,
}

fn spawn(registry: Arc<Registry>, buffer: BufferConfig, pull: UdpPull) -> Entry {
    let p = pull.clone();
    Entry { pull, task: tokio::spawn(run(p, registry, buffer)) }
}

/// Starts every pull. Must be called inside a tokio runtime.
pub fn start_pulls(registry: Arc<Registry>, buffer: BufferConfig, pulls: Vec<UdpPull>) -> UdpPullHandle {
    let entries = pulls.into_iter().map(|p| spawn(registry.clone(), buffer, p)).collect();
    UdpPullHandle { entries: Arc::new(Mutex::new(entries)), buffer }
}

impl UdpPullHandle {
    /// Unchanged pulls keep their socket; removed ones stop (ending their
    /// stream), new or changed ones start.
    pub fn reload(&self, registry: &Arc<Registry>, pulls: Vec<UdpPull>) {
        let mut entries = self.entries.lock();
        let mut remaining = std::mem::take(&mut *entries);
        let mut next = Vec::with_capacity(pulls.len());
        for p in pulls {
            match remaining.iter().position(|e| e.pull == p) {
                Some(pos) => next.push(remaining.remove(pos)),
                None => next.push(spawn(registry.clone(), self.buffer, p)),
            }
        }
        for gone in remaining {
            gone.task.abort();
        }
        *entries = next;
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        let t = parse_url("udp://239.1.1.1:5000").unwrap();
        assert!(t.is_multicast() && !t.rtp && t.addr.port() == 5000);
        assert_eq!(parse_url("udp://@239.1.1.1:5000").unwrap().addr, "239.1.1.1:5000".parse().unwrap(), "VLC form");
        assert_eq!(parse_url("udp://:5000").unwrap().addr, "0.0.0.0:5000".parse().unwrap());
        assert!(parse_url("rtp://239.1.1.2:5004").unwrap().rtp);
        assert!(parse_url("udp://[ff15::1]:5000").unwrap().is_multicast());
        assert!(parse_url("udp://239.1.1.1").is_err(), "port required");
        assert!(parse_url("udp://encoder.local:5000").is_err(), "addresses, not names");
        assert!(parse_url("srt://h:1").is_err());
        assert!(parse_url("udp://:0").is_err());
    }

    #[test]
    fn rtp_payloads() {
        let ts = [0x47u8; 188];
        let mut pkt = vec![0x80, 33, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1];
        pkt.extend_from_slice(&ts);
        assert_eq!(ts_payload(&pkt, true).unwrap().len(), 188);
        assert_eq!(ts_payload(&pkt, false).unwrap().len(), 200, "plain UDP takes everything");
        // One CSRC, an extension of one word, and 4 bytes of padding.
        let mut p2 = vec![0xB1, 33, 0, 2, 0, 0, 0, 0, 0, 0, 0, 1, 9, 9, 9, 9, 0xBE, 0xDE, 0, 1, 7, 7, 7, 7];
        p2.extend_from_slice(&ts);
        p2.extend_from_slice(&[0, 0, 0, 4]);
        assert_eq!(ts_payload(&p2, true).unwrap(), &ts[..]);
        assert!(ts_payload(&[0x40, 0, 0], true).is_none(), "not RTP v2");
    }
}
