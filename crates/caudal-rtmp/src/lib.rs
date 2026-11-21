//! RTMP / Enhanced RTMP ingest: accepts publishers ([`serve`]) and pulls
//! from remote RTMP servers as a player ([`pull`], `[[rtmp.pull]]`), feeding
//! a [`caudal_core::Publisher`] either way. Also home of the RTMP/RTMPS
//! client ([`client`]) `caudal-restream` pushes with.
//!
//! Built directly on `scuffle-rtmp`'s [`ServerSession`](scuffle_rtmp::ServerSession),
//! driven from our own `tokio::net::TcpListener` (its `SessionHandler` API is
//! transport-agnostic, so no fallback to `rml_rtmp` was needed). FLV tag
//! parsing uses `scuffle-flv`, `scuffle-h264`, `scuffle-h265`, `scuffle-av1`,
//! `scuffle-amf0` and `scuffle-aac`. See `NOTES.md` for the raw-byte-fidelity
//! design of `crate::demux`.
//!
//! RTMPS publish ([`serve_tls`]) is the same session code over a
//! `tokio_rustls` stream: TLS is terminated at accept, then the RTMP
//! handshake and chunk stream run inside it unchanged.

use std::net::SocketAddr;
use std::sync::Arc;

use caudal_core::{BufferConfig, Registry};
use scuffle_rtmp::ServerSession;
use tokio::net::TcpListener;

pub mod client;
mod demux;
pub mod ingest;
pub mod pull;
mod session;
pub mod url;

pub use pull::{RtmpPull, RtmpPullHandle, start_pulls};
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

/// How long a client gets to finish the TLS handshake before the
/// connection is dropped (a port scanner or a stuck client must not hold a
/// socket and a task forever).
const TLS_HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Listens until the future is dropped or the socket fails.
pub async fn serve(cfg: RtmpConfig, registry: Arc<Registry>) -> std::io::Result<()> {
    let listener = TcpListener::bind(cfg.bind).await?;
    tracing::info!(bind = %cfg.bind, app = %cfg.app, "rtmp listening");
    accept_loop(listener, cfg, registry, None).await
}

/// RTMPS publish: like [`serve`], but every connection is a TLS server
/// handshake first (`cfg.bind` is the TLS address, e.g. `0.0.0.0:1936` or
/// `:443`). Encoders publish to `rtmps://host:port/{app}/{stream}`. The
/// acceptor comes from `caudal_tls::tls_acceptor`, so certificates follow
/// `[tls]` (reloaded files or ACME) without restarting this listener.
pub async fn serve_tls(
    cfg: RtmpConfig,
    acceptor: tokio_rustls::TlsAcceptor,
    registry: Arc<Registry>,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(cfg.bind).await?;
    tracing::info!(bind = %cfg.bind, app = %cfg.app, "rtmps listening");
    accept_loop(listener, cfg, registry, Some(acceptor)).await
}

async fn accept_loop(
    listener: TcpListener,
    cfg: RtmpConfig,
    registry: Arc<Registry>,
    tls: Option<tokio_rustls::TlsAcceptor>,
) -> std::io::Result<()> {
    let scheme = if tls.is_some() { "rtmps" } else { "rtmp" };
    loop {
        // One accept error (EMFILE when out of file descriptors, a
        // connection reset before accept) must not stop the listener for
        // good while /healthz keeps saying OK. Same policy as axum::serve.
        let (stream, addr) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                tracing::warn!(error = %e, scheme, "rtmp accept failed; retrying");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };
        let registry = registry.clone();
        let app = cfg.app.clone();
        let buffer = cfg.buffer;
        let tls = tls.clone();

        // Each connection runs on its own task: a panic, a slow client or a
        // malformed handshake in one never affects any other publisher.
        tokio::spawn(async move {
            let handler = Handler::new(registry, app, buffer, addr.ip());
            let result = match tls {
                None => ServerSession::new(stream, handler).run().await,
                Some(acceptor) => match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
                    Ok(Ok(tls_stream)) => ServerSession::new(tls_stream, handler).run().await,
                    Ok(Err(err)) => {
                        tracing::debug!(%addr, %err, "rtmps tls handshake failed");
                        return;
                    }
                    Err(_) => {
                        tracing::debug!(%addr, "rtmps tls handshake timed out");
                        return;
                    }
                },
            };
            match result {
                Ok(clean) => tracing::debug!(%addr, clean, scheme, "rtmp session ended"),
                Err(err) => tracing::debug!(%addr, %err, scheme, "rtmp session ended with error"),
            }
        });
    }
}
