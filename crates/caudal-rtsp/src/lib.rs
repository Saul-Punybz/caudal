//! RTSP in and out. Entry point fixed by the orchestrator.
//!
//! - Pull ([`start_pulls`]): each [`RtspPull`] connects to a camera
//!   (`rtsp://user:pass@cam/...`) over TCP interleaved with `retina` and
//!   publishes it as `stream` (AVCC video, avcC/hvcC init, raw AAC audio),
//!   reconnecting forever with backoff. Runs as its own subsystem,
//!   independent of [`serve`], so a `[[rtsp.pull]]` reload
//!   ([`PullHandle::reload`]) never touches the RTSP server or any
//!   already-connected client.
//! - Serve ([`server`]): when `bind` is set, `rtsp://host:port/<stream>`
//!   (optional `?token=`) plays any live stream: OPTIONS, DESCRIBE (SDP
//!   built in [`sdp`]), SETUP, PLAY, TEARDOWN, GET_PARAMETER, with
//!   `Access::Play` checked. Media travels TCP interleaved or UDP unicast
//!   (SETUP picks per track; a UDP port pair comes from `udp_port_range`).
//!   When `tls` is set, the same server also answers `rtsps://` on a
//!   second bind, TLS from `caudal-tls`, TCP interleaved only. Packetizing
//!   (H.264 FU-A, RFC 3640 AAC AU headers) lives in [`rtp`]; RTCP Sender
//!   Reports for UDP tracks live in [`rtcp`].

mod pull;
mod rtcp;
mod rtp;
mod sdp;
mod server;
mod udp;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use caudal_core::{BufferConfig, Registry};
use parking_lot::Mutex;

/// RFC 2326 §12.37's usual default for `RtspConfig::session_timeout`.
pub const DEFAULT_SESSION_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtspPull {
    pub stream: String,
    pub url: String,
}

struct PullEntry {
    pull: RtspPull,
    task: tokio::task::JoinHandle<()>,
}

/// A handle to the running `[[rtsp.pull]]` tasks. Cheap to clone.
#[derive(Clone)]
pub struct PullHandle {
    entries: Arc<Mutex<Vec<PullEntry>>>,
}

fn spawn_pull(registry: Arc<Registry>, buffer: BufferConfig, pull: RtspPull) -> PullEntry {
    let task_pull = pull.clone();
    let task = tokio::spawn(async move { pull::run(task_pull, registry, buffer).await });
    PullEntry { pull, task }
}

/// Starts every configured camera pull, reconnecting forever, and returns
/// immediately. Must be called inside a tokio runtime: each pull is its own
/// task, so one camera never affects any other, the RTSP server, or any
/// other ingest/output subsystem.
pub fn start_pulls(registry: Arc<Registry>, buffer: BufferConfig, pulls: Vec<RtspPull>) -> PullHandle {
    let entries = pulls.into_iter().map(|p| spawn_pull(registry.clone(), buffer, p)).collect();
    PullHandle { entries: Arc::new(Mutex::new(entries)) }
}

impl PullHandle {
    /// Applies a new pull list: a pull whose `stream` and `url` are both
    /// unchanged keeps its task (and its live camera connection)
    /// untouched; removed pulls are stopped (their stream ends), changed
    /// or new ones (re)started.
    pub fn reload(&self, registry: &Arc<Registry>, buffer: BufferConfig, pulls: Vec<RtspPull>) {
        let mut entries = self.entries.lock();
        let mut remaining = std::mem::take(&mut *entries);
        let mut next = Vec::with_capacity(pulls.len());
        for p in pulls {
            if let Some(pos) = remaining.iter().position(|e| e.pull == p) {
                next.push(remaining.remove(pos));
            } else {
                next.push(spawn_pull(registry.clone(), buffer, p));
            }
        }
        for gone in remaining {
            gone.task.abort();
        }
        *entries = next;
    }

    /// Stops every pull task; their streams end.
    pub fn stop(&self) {
        for e in self.entries.lock().drain(..) {
            e.task.abort();
        }
    }
}

/// RTSPS: a second bind speaking the same RTSP control protocol over TLS
/// (TCP interleaved media only; see `server` module docs for why).
#[derive(Debug, Clone)]
pub struct RtspTlsConfig {
    pub bind: SocketAddr,
    pub cert: PathBuf,
    pub key: PathBuf,
}

#[derive(Debug, Clone)]
pub struct RtspConfig {
    /// RTSP server address; `None` disables serving.
    pub bind: Option<SocketAddr>,
    pub buffer: BufferConfig,
    /// RTSPS alongside `bind`; `None` disables it.
    pub tls: Option<RtspTlsConfig>,
    /// `(start, end)`: the range SETUP allocates RTP/RTCP port pairs from
    /// for UDP unicast playback.
    pub udp_port_range: (u16, u16),
    /// How long an idle session survives (any RTSP request, or an incoming
    /// RTCP receiver report on a UDP track, resets the clock); advertised
    /// as `Session: ...;timeout=<secs>`. [`DEFAULT_SESSION_TIMEOUT`] unless
    /// a caller needs something shorter (tests mostly).
    pub session_timeout: Duration,
}

/// Fuzz-only entry points into the request-parsing internals of
/// [`crate::server`], which are otherwise private. Not part of the public
/// API; used by `fuzz/fuzz_targets/rtsp_request.rs`. Must never panic on
/// any input.
#[doc(hidden)]
pub mod fuzz {
    /// Parses `data` as an RTSP message, and, if it is a request, runs the
    /// same URI, `Transport` and `Session` header parsing `server::handle_setup`
    /// and `server::handle_play` do before ever touching a registry or socket.
    pub fn route_request(data: &[u8]) {
        let Ok((rtsp_types::Message::Request(req), _)) = rtsp_types::Message::<Vec<u8>>::parse(data) else {
            return;
        };
        let _ = crate::server::parse_uri(&req);
        if let Ok(Some(transports)) = req.typed_header::<rtsp_types::headers::Transports>() {
            let _ = crate::server::choose_transport(&transports, false);
            let _ = crate::server::choose_transport(&transports, true);
        }
        let _ = req.typed_header::<rtsp_types::headers::Session>();
    }
}

/// Runs the RTSP server until dropped; `[[rtsp.pull]]` cameras run
/// separately, see [`start_pulls`]. If `bind` is `None` this never returns
/// (nothing to serve), so callers only spawn it when `bind.is_some()`.
pub async fn serve(cfg: RtspConfig, registry: Arc<Registry>) -> std::io::Result<()> {
    match cfg.bind {
        Some(bind) => server::serve(bind, cfg.tls, cfg.udp_port_range, cfg.session_timeout, registry).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod reload_tests {
    use super::*;

    fn pull(stream: &str, url: &str) -> RtspPull {
        RtspPull { stream: stream.into(), url: url.into() }
    }

    fn task_ids(handle: &PullHandle) -> Vec<tokio::task::Id> {
        handle.entries.lock().iter().map(|e| e.task.id()).collect()
    }

    #[tokio::test]
    async fn unchanged_pull_keeps_its_task() {
        let registry = Registry::new();
        let handle = start_pulls(registry.clone(), BufferConfig::default(), vec![pull("cam1", "rtsp://x/1")]);
        let before = task_ids(&handle);
        handle.reload(&registry, BufferConfig::default(), vec![pull("cam1", "rtsp://x/1")]);
        assert_eq!(task_ids(&handle), before, "unchanged pull must not be restarted");
    }

    #[tokio::test]
    async fn changed_removed_and_added_pulls() {
        let registry = Registry::new();
        let handle = start_pulls(
            registry.clone(),
            BufferConfig::default(),
            vec![pull("cam1", "rtsp://x/1"), pull("cam2", "rtsp://x/2")],
        );
        let before = task_ids(&handle);

        handle.reload(
            &registry,
            BufferConfig::default(),
            vec![pull("cam1", "rtsp://x/CHANGED"), pull("cam3", "rtsp://x/3")],
        );

        let after = handle.entries.lock();
        assert_eq!(after.len(), 2);
        assert!(after.iter().any(|e| e.pull.stream == "cam3"));
        assert!(!after.iter().any(|e| e.pull.stream == "cam2"), "removed pull is gone");
        let cam1_task = after.iter().find(|e| e.pull.stream == "cam1").unwrap().task.id();
        assert!(!before.contains(&cam1_task), "changed pull got a new task");
    }
}
