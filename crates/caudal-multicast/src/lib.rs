//! IP multicast output: sends a live stream as MPEG-TS over UDP, or as
//! RTP/MP2T (RFC 2250, payload type 33), to a multicast group. On cable/
//! IPTV, campus and hotel networks one copy leaves the server and the
//! switches replicate it to every receiver that joined the group (IGMP);
//! see `docs/MULTICAST.md` for the network side.
//!
//! One task per configured [`MulticastTarget`] (`crate::send`): it waits
//! for the stream, muxes it with `caudal_ts::mux::TsMux` in media-clock
//! mode (PAT/PMT every 100 ms of media time and on every keyframe, a PCR
//! on every video frame), groups the TS packets seven to a datagram
//! (`crate::packet`), and paces the datagrams to the stream's own clock
//! with a small constant-rate smoother (`crate::pacer`) instead of letting
//! a keyframe leave as one burst. The socket (`crate::socket`) sets
//! `IP_MULTICAST_TTL`, `IP_MULTICAST_IF` and `IP_MULTICAST_LOOP` (or their
//! IPv6 counterparts). The output starts when the stream goes live, stops
//! cleanly when it ends, and rejoins on the next publish.
//!
//! `GET /api/v1/multicast` lists each output's counters;
//! [`MulticastHandle::render_metrics`] appends them to `/metrics`.

mod http;
mod pacer;
mod packet;
mod send;
mod socket;
mod status;

use std::fmt::Write as _;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use caudal_core::Registry;
use parking_lot::Mutex;
use status::OutputStatus;

/// Payload format on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    /// Raw MPEG-TS: seven 188-byte packets per UDP datagram
    /// (`udp://@group:port` in VLC, ffmpeg, set-top boxes).
    #[default]
    Ts,
    /// RTP with an MP2T payload (RFC 2250, PT 33): the same seven packets
    /// behind a 12-byte RTP header, so receivers see loss and reordering
    /// (`rtp://@group:port`).
    Rtp,
}

impl Format {
    pub fn as_str(self) -> &'static str {
        match self {
            Format::Ts => "ts",
            Format::Rtp => "rtp",
        }
    }
}

/// Which interface multicast leaves on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Interface {
    /// The OS picks (the route to the group, usually the default route).
    #[default]
    Default,
    /// IPv4: the interface with this address (`IP_MULTICAST_IF`), which is
    /// also the datagrams' source address.
    V4(Ipv4Addr),
    /// IPv6: the interface with this index (`IPV6_MULTICAST_IF`).
    V6Index(u32),
}

/// Send `stream` to the multicast `group` while it is live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MulticastTarget {
    pub stream: String,
    pub group: SocketAddr,
    pub format: Format,
    /// IP TTL / IPv6 hop limit: how many routers the datagrams may cross.
    pub ttl: u32,
    pub interface: Interface,
    /// `IP_MULTICAST_LOOP`: also deliver to receivers on this host.
    pub loopback: bool,
    /// Pace to the stream's clock (on by default; off sends each frame's
    /// datagrams the moment they are muxed).
    pub pacing: bool,
}

impl MulticastTarget {
    /// Checks everything that would otherwise only fail once the stream is
    /// live: a multicast group address, a port, a TTL that fits, an
    /// interface of the group's address family.
    pub fn validate(&self) -> Result<(), String> {
        let group = self.group;
        if self.stream.is_empty() {
            return Err(format!("multicast {group}: `stream` is empty"));
        }
        if !group.ip().is_multicast() {
            return Err(format!(
                "multicast group {group} is not a multicast address (IPv4 224.0.0.0/4, IPv6 ff00::/8)"
            ));
        }
        if group.port() == 0 {
            return Err(format!("multicast group {group}: port 0"));
        }
        if self.ttl == 0 || self.ttl > 255 {
            return Err(format!("multicast group {group}: ttl must be 1..=255, got {}", self.ttl));
        }
        match (group, self.interface) {
            (_, Interface::Default)
            | (SocketAddr::V4(_), Interface::V4(_))
            | (SocketAddr::V6(_), Interface::V6Index(_)) => Ok(()),
            (SocketAddr::V4(_), Interface::V6Index(_)) => {
                Err(format!("multicast group {group}: an IPv4 group needs an IPv4 interface address"))
            }
            (SocketAddr::V6(_), Interface::V4(_)) => {
                Err(format!("multicast group {group}: an IPv6 group needs a numeric interface index"))
            }
        }
    }
}

struct Entry {
    target: MulticastTarget,
    status: Arc<OutputStatus>,
    task: tokio::task::JoinHandle<()>,
}

/// The running outputs; also the `axum` state for [`router`]. Cheap to
/// clone.
#[derive(Clone)]
pub struct MulticastHandle {
    entries: Arc<Mutex<Vec<Entry>>>,
}

fn spawn(registry: Arc<Registry>, target: MulticastTarget) -> Entry {
    let status = Arc::new(OutputStatus::new(&target));
    let (task_target, task_status) = (target.clone(), status.clone());
    let task = tokio::spawn(async move { send::run(task_target, registry, task_status).await });
    Entry { target, status, task }
}

/// Starts every output (each waits for its stream to be published) and
/// returns immediately. Must run inside a tokio runtime.
pub fn start(registry: Arc<Registry>, targets: Vec<MulticastTarget>) -> MulticastHandle {
    let entries = targets.into_iter().map(|t| spawn(registry.clone(), t)).collect();
    MulticastHandle { entries: Arc::new(Mutex::new(entries)) }
}

impl MulticastHandle {
    /// Applies a new output list: unchanged outputs keep running untouched
    /// (their counters survive), removed ones stop, new or changed ones
    /// (re)start.
    pub fn reload(&self, registry: &Arc<Registry>, targets: Vec<MulticastTarget>) {
        let mut entries = self.entries.lock();
        let mut remaining = std::mem::take(&mut *entries);
        let mut next = Vec::with_capacity(targets.len());
        for target in targets {
            match remaining.iter().position(|e| e.target == target) {
                Some(pos) => next.push(remaining.remove(pos)),
                None => next.push(spawn(registry.clone(), target)),
            }
        }
        for gone in remaining {
            gone.task.abort();
        }
        *entries = next;
    }

    fn statuses(&self) -> Vec<status::OutputJson> {
        self.entries.lock().iter().map(|e| e.status.to_json()).collect()
    }

    /// Appends the `caudal_multicast_*` series to a Prometheus text body.
    /// Writes nothing when no output is configured.
    pub fn render_metrics(&self, out: &mut String) {
        let rows = self.statuses();
        if rows.is_empty() {
            return;
        }
        let series: [Series; 4] = [
            ("caudal_multicast_packets_total", "counter", "Datagrams sent to a multicast group.", |r| {
                r.packets_sent.to_string()
            }),
            ("caudal_multicast_bytes_total", "counter", "UDP payload bytes sent to a multicast group.", |r| {
                r.bytes_sent.to_string()
            }),
            (
                "caudal_multicast_send_errors_total",
                "counter",
                "Datagrams to a multicast group that failed to send.",
                |r| r.send_errors.to_string(),
            ),
            (
                "caudal_multicast_pacing_lag_seconds",
                "gauge",
                "How late the last datagram left relative to its place on the stream's clock.",
                |r| (r.pacing_lag_ms / 1000.0).to_string(),
            ),
        ];
        for (name, kind, help, value) in series {
            let _ = writeln!(out, "# HELP {name} {help}");
            let _ = writeln!(out, "# TYPE {name} {kind}");
            for r in &rows {
                let _ = writeln!(
                    out,
                    "{name}{{stream=\"{}\",group=\"{}\",format=\"{}\"}} {}",
                    escape(&r.stream),
                    escape(&r.group),
                    r.format,
                    value(r)
                );
            }
        }
    }
}

/// (metric name, type, help text, value of one row).
type Series = (&'static str, &'static str, &'static str, fn(&status::OutputJson) -> String);

/// Escapes a Prometheus label value.
fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

/// `GET /api/v1/multicast`: one row per configured output.
pub fn router(handle: MulticastHandle) -> axum::Router {
    http::router(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(stream: &str, group: &str) -> MulticastTarget {
        MulticastTarget {
            stream: stream.into(),
            group: group.parse().unwrap(),
            format: Format::Ts,
            ttl: 16,
            interface: Interface::Default,
            loopback: false,
            pacing: true,
        }
    }

    #[test]
    fn validates_groups() {
        assert!(target("a", "239.1.1.1:5000").validate().is_ok());
        assert!(target("a", "[ff15::1]:5000").validate().is_ok());
        assert!(target("a", "10.0.0.1:5000").validate().unwrap_err().contains("not a multicast"));
        assert!(target("a", "239.1.1.1:0").validate().unwrap_err().contains("port 0"));
        assert!(target("", "239.1.1.1:5000").validate().is_err());
        let mut t = target("a", "239.1.1.1:5000");
        t.ttl = 0;
        assert!(t.validate().is_err());
        t.ttl = 256;
        assert!(t.validate().is_err());
        t.ttl = 1;
        t.interface = Interface::V6Index(2);
        assert!(t.validate().is_err());
        let mut t = target("a", "[ff15::1]:5000");
        t.interface = Interface::V4(Ipv4Addr::LOCALHOST);
        assert!(t.validate().is_err());
    }

    fn task_ids(h: &MulticastHandle) -> Vec<tokio::task::Id> {
        h.entries.lock().iter().map(|e| e.task.id()).collect()
    }

    #[tokio::test]
    async fn reload_keeps_unchanged_outputs() {
        let registry = Registry::new();
        let h = start(registry.clone(), vec![target("a", "239.1.1.1:5000"), target("b", "239.1.1.2:5000")]);
        let before = task_ids(&h);
        h.reload(&registry, vec![target("a", "239.1.1.1:5000"), target("c", "239.1.1.3:5000")]);
        let after = task_ids(&h);
        assert_eq!(after.len(), 2);
        assert_eq!(after[0], before[0], "unchanged output keeps its task");
        assert!(!before.contains(&after[1]), "new output gets a new task");
        let rows = h.statuses();
        assert_eq!(rows[1].stream, "c");
        assert_eq!(rows[0].state, "waiting");
    }

    #[tokio::test]
    async fn metrics_render_per_output() {
        let registry = Registry::new();
        let h = start(registry.clone(), vec![target("tv\"1", "239.1.1.1:5000")]);
        let mut body = String::new();
        h.render_metrics(&mut body);
        assert!(
            body.contains(
                "caudal_multicast_packets_total{stream=\"tv\\\"1\",group=\"239.1.1.1:5000\",format=\"ts\"} 0"
            ),
            "{body}"
        );
        assert!(body.contains("# TYPE caudal_multicast_pacing_lag_seconds gauge"), "{body}");
        let mut empty = String::new();
        start(registry, Vec::new()).render_metrics(&mut empty);
        assert!(empty.is_empty());
    }
}
