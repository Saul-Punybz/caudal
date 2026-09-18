//! Media over QUIC output: every live stream in the registry is published as
//! a `hang` broadcast named after the stream, served over QUIC/WebTransport
//! by an embedded relay. Entry point fixed by the orchestrator.
//!
//! The moq-dev crates move fast; they are pinned to exact versions.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use caudal_core::Registry;

#[derive(Debug, Clone)]
pub enum MoqCert {
    /// PEM chain + key (e.g. the same files as `[tls]`).
    Files { cert: PathBuf, key: PathBuf },
    /// Generate an ECDSA P-256 certificate valid < 14 days for `hosts`, the
    /// kind browsers accept via WebTransport `serverCertificateHashes`;
    /// regenerated before it expires.
    SelfSigned { hosts: Vec<String> },
}

#[derive(Debug, Clone)]
pub struct MoqConfig {
    /// UDP address for QUIC.
    pub bind: SocketAddr,
    pub cert: MoqCert,
}

/// The running MoQ output.
pub struct MoqService {
    _private: (),
}

impl MoqService {
    /// Serves `GET /moq/fingerprint` (JSON `{"url": "https://host:port",
    /// "fingerprint": "<sha-256 hex>" | null}`) so players can connect to a
    /// self-signed endpoint. Merge into the HTTP app.
    pub fn router(&self) -> axum::Router {
        axum::Router::new()
    }
}

/// Binds QUIC and starts publishing every current and future stream. Must
/// be called inside a tokio runtime.
pub fn start(registry: Arc<Registry>, cfg: MoqConfig) -> std::io::Result<MoqService> {
    let _ = (registry, cfg);
    Err(std::io::Error::other("MoQ not implemented yet"))
}
