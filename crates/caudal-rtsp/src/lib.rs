//! RTSP in and out. Entry point fixed by the orchestrator.
//!
//! - Pull ([`pull`]): each [`RtspPull`] connects to a camera
//!   (`rtsp://user:pass@cam/...`) over TCP interleaved with `retina` and
//!   publishes it as `stream` (AVCC video, avcC/hvcC init, raw AAC audio),
//!   reconnecting forever with backoff.
//! - Serve ([`server`]): when `bind` is set, `rtsp://host:port/<stream>`
//!   (optional `?token=`) plays any live stream over TCP interleaved:
//!   OPTIONS, DESCRIBE (SDP built in [`sdp`]), SETUP, PLAY, TEARDOWN,
//!   GET_PARAMETER, with `Access::Play` checked. Packetizing (H.264 FU-A,
//!   RFC 3640 AAC AU headers) lives in [`rtp`].

mod pull;
mod rtp;
mod sdp;
mod server;

use std::net::SocketAddr;
use std::sync::Arc;

use caudal_core::{BufferConfig, Registry};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtspPull {
    pub stream: String,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct RtspConfig {
    /// RTSP server address; `None` disables serving.
    pub bind: Option<SocketAddr>,
    pub pulls: Vec<RtspPull>,
    pub buffer: BufferConfig,
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
        Some(bind) => server::serve(bind, registry).await,
        None => std::future::pending().await,
    }
}
