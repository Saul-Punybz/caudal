//! SRT ingest: accepts SRT callers carrying MPEG-TS and feeds a
//! [`caudal_core::Publisher`]. Entry point fixed by the orchestrator.

use std::net::SocketAddr;
use std::sync::Arc;

use caudal_core::{BufferConfig, Registry};

#[derive(Debug, Clone)]
pub struct SrtConfig {
    pub bind: SocketAddr,
    /// Receiver latency (TSBPD), milliseconds.
    pub latency_ms: u32,
    /// When set, callers must use this passphrase (AES).
    pub passphrase: Option<String>,
    pub buffer: BufferConfig,
}

/// Listens until the future is dropped or the socket fails. A caller's
/// stream id selects the stream: `publish/<name>`, or the SRT access-control
/// form `#!::r=<name>,m=publish`.
pub async fn serve(cfg: SrtConfig, registry: Arc<Registry>) -> std::io::Result<()> {
    let _ = (cfg, registry);
    Err(std::io::Error::other("SRT ingest not implemented yet"))
}
