//! SRT ingest: accepts SRT callers carrying MPEG-TS and feeds a
//! [`caudal_core::Publisher`]. Entry point fixed by the orchestrator.
//!
//! Built on `rsrt` (pure-Rust SRT, live mode, verified against libsrt
//! 1.5.6) for the transport and `mpeg2ts` for TS/PES parsing; see
//! `NOTES.md` for the demux reuse decision and the AVCC/hvcC/ADTS
//! conversion rules. One task per accepted connection: a malformed stream
//! or a rejected caller only ever affects its own connection.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use caudal_core::{BufferConfig, Registry};
use rsrt::{SrtListener, SrtOptions};

mod connection;
mod demux;
mod ts;

#[derive(Debug, Clone)]
pub struct SrtConfig {
    pub bind: SocketAddr,
    /// Receiver latency (TSBPD), milliseconds.
    pub latency_ms: u32,
    /// When set, callers must use this passphrase (AES).
    pub passphrase: Option<String>,
    pub buffer: BufferConfig,
    /// Streams to push out to remote SRT listeners (caller mode).
    pub pushes: Vec<SrtPush>,
}

/// Push `stream` to `url` (`srt://host:port?streamid=...&passphrase=...`),
/// reconnecting while the stream is live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SrtPush {
    pub stream: String,
    pub url: String,
}

/// Listens until the future is dropped or the socket fails. A caller's
/// stream id selects the stream and direction: `publish/<name>` or
/// `#!::r=<name>,m=publish` to send into Caudal; `play/<name>` or
/// `#!::r=<name>,m=request` to receive a stream as MPEG-TS (batch 6).
/// Also runs the configured `pushes`.
pub async fn serve(cfg: SrtConfig, registry: Arc<Registry>) -> std::io::Result<()> {
    // A live publisher sends media continuously, so 3 s without any means it
    // is gone. A caller killed outright never sends SRT's shutdown, and the
    // default 5 s peer-idle timer (keepalives count) ends the stream late.
    let mut opts = SrtOptions::default()
        .latency(Duration::from_millis(u64::from(cfg.latency_ms)))
        .data_idle_timeout(Duration::from_secs(3));
    if let Some(passphrase) = cfg.passphrase.clone() {
        opts = opts.passphrase(passphrase);
    }

    // Encryption is enforced by `rsrt` at the handshake itself (both-or-
    // neither passphrase, and it must be the right one): a mismatched
    // caller is rejected before `accept()` ever sees it, so no extra
    // passphrase check is needed here.
    let mut listener = SrtListener::bind(cfg.bind, opts)
        .await
        .map_err(|err| std::io::Error::other(format!("srt bind failed: {err}")))?;
    tracing::info!(bind = %cfg.bind, "srt listening");

    loop {
        let (socket, peer) =
            listener.accept().await.map_err(|err| std::io::Error::other(format!("srt accept failed: {err}")))?;
        let registry = registry.clone();
        let buffer = cfg.buffer;
        // Each connection runs on its own task: a panic, a slow client or a
        // malformed stream in one never affects any other publisher.
        tokio::spawn(async move {
            connection::handle(socket, peer, registry, buffer).await;
        });
    }
}
