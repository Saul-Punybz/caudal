//! RTSP in and out. Entry point fixed by the orchestrator.
//!
//! - Pull ([`pull`]): each [`RtspPull`] connects to a camera
//!   (`rtsp://user:pass@cam/...`) over TCP interleaved with `retina` and
//!   publishes it as `stream` (AVCC video, avcC/hvcC init, raw AAC audio),
//!   reconnecting forever with backoff.
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

/// RFC 2326 §12.37's usual default for `RtspConfig::session_timeout`.
pub const DEFAULT_SESSION_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtspPull {
    pub stream: String,
    pub url: String,
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
    pub pulls: Vec<RtspPull>,
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

/// Runs pulls and the server until dropped. Pulls never return (they
/// reconnect forever); if `bind` is `None` this simply waits on them.
pub async fn serve(cfg: RtspConfig, registry: Arc<Registry>) -> std::io::Result<()> {
    for p in cfg.pulls {
        let registry = registry.clone();
        let buffer = cfg.buffer;
        tokio::spawn(async move { pull::run(p, registry, buffer).await });
    }

    match cfg.bind {
        Some(bind) => server::serve(bind, cfg.tls, cfg.udp_port_range, cfg.session_timeout, registry).await,
        None => std::future::pending().await,
    }
}
