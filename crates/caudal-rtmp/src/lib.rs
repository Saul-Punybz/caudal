//! RTMP / Enhanced RTMP ingest: accepts publishers and feeds a
//! [`caudal_core::Publisher`]. Entry point fixed by the orchestrator.

use std::net::SocketAddr;
use std::sync::Arc;

use caudal_core::{BufferConfig, Registry};

#[derive(Debug, Clone)]
pub struct RtmpConfig {
    pub bind: SocketAddr,
    /// Only `rtmp://host/{app}/{stream}` is accepted.
    pub app: String,
    pub buffer: BufferConfig,
}

/// Listens until the future is dropped or the socket fails.
pub async fn serve(cfg: RtmpConfig, registry: Arc<Registry>) -> std::io::Result<()> {
    let _ = (cfg, registry);
    todo!("agent B")
}
