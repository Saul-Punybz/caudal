//! RTSP in and out. Entry point fixed by the orchestrator.
//!
//! - Pull: each [`RtspPull`] connects to a camera (`rtsp://user:pass@cam/...`)
//!   with `retina` and publishes it as `stream`, reconnecting forever.
//! - Serve: when `bind` is set, `rtsp://host:port/<stream>` (optional
//!   `?token=`) plays any live stream: DESCRIBE, SETUP (TCP interleaved;
//!   UDP optional), PLAY, TEARDOWN, with `Access::Play` checked.

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

/// Runs pulls and the server until dropped.
pub async fn serve(cfg: RtspConfig, registry: Arc<Registry>) -> std::io::Result<()> {
    let _ = (cfg, registry);
    Err(std::io::Error::other("RTSP not implemented yet"))
}
