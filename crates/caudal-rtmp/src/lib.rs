//! RTMP / Enhanced RTMP ingest: accepts publishers and feeds a
//! [`caudal_core::Publisher`]. Entry point fixed by the orchestrator.
//!
//! Built directly on `scuffle-rtmp`'s [`ServerSession`](scuffle_rtmp::ServerSession),
//! driven from our own `tokio::net::TcpListener` (its `SessionHandler` API is
//! transport-agnostic, so no fallback to `rml_rtmp` was needed). FLV tag
//! parsing uses `scuffle-flv`, `scuffle-h264`, `scuffle-h265`, `scuffle-av1`,
//! `scuffle-amf0` and `scuffle-aac`. See `NOTES.md` for the raw-byte-fidelity
//! design of `crate::demux`.

use std::net::SocketAddr;
use std::sync::Arc;

use caudal_core::{BufferConfig, Registry};
use scuffle_rtmp::ServerSession;
use tokio::net::TcpListener;

mod demux;
mod session;

use session::Handler;

#[derive(Debug, Clone)]
pub struct RtmpConfig {
    pub bind: SocketAddr,
    /// Only `rtmp://host/{app}/{stream}` is accepted.
    pub app: String,
    pub buffer: BufferConfig,
}

/// Fuzz-only entry points into the FLV/AMF parsing internals of
/// [`crate::demux`], which are otherwise `pub(crate)`. Not part of the
/// public API; used by `fuzz/fuzz_targets/rtmp_flv_amf.rs`. Every function
/// here must never panic, only return `None`/drop malformed input.
#[doc(hidden)]
pub mod fuzz {
    use bytes::Bytes;

    pub fn demux_video(timestamp_ms: i64, data: Bytes) {
        let _ = crate::demux::demux_video(timestamp_ms, data);
    }

    pub fn demux_audio(data: Bytes) {
        let _ = crate::demux::demux_audio(data);
    }

    pub fn parse_cue_point(timestamp_ms: i64, data: Bytes, event_id: u32) {
        let _ = crate::demux::parse_cue_point(timestamp_ms, data, event_id);
    }

    pub fn parse_metadata_fps(data: Bytes) {
        let _ = crate::demux::parse_metadata_fps(data);
    }
}

/// Listens until the future is dropped or the socket fails.
pub async fn serve(cfg: RtmpConfig, registry: Arc<Registry>) -> std::io::Result<()> {
    let listener = TcpListener::bind(cfg.bind).await?;
    tracing::info!(bind = %cfg.bind, app = %cfg.app, "rtmp listening");

    loop {
        // One accept error (EMFILE when out of file descriptors, a
        // connection reset before accept) must not stop the listener for
        // good while /healthz keeps saying OK. Same policy as axum::serve.
        let (stream, addr) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                tracing::warn!(error = %e, "rtmp accept failed; retrying");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };
        let registry = registry.clone();
        let app = cfg.app.clone();
        let buffer = cfg.buffer;

        // Each connection runs on its own task: a panic, a slow client or a
        // malformed handshake in one never affects any other publisher.
        tokio::spawn(async move {
            let handler = Handler::new(registry, app, buffer, addr.ip());
            let session = ServerSession::new(stream, handler);
            match session.run().await {
                Ok(clean) => tracing::debug!(%addr, clean, "rtmp session ended"),
                Err(err) => tracing::debug!(%addr, %err, "rtmp session ended with error"),
            }
        });
    }
}
