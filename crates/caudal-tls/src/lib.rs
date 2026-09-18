//! HTTPS for Caudal: HTTP/1.1 and HTTP/2 negotiated by ALPN, certificates
//! from files (reloaded when they change) or from Let's Encrypt (ACME).
//! Entry point fixed by the orchestrator.

use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub enum CertSource {
    /// PEM certificate chain and private key, re-read when either changes.
    Files { cert: PathBuf, key: PathBuf },
    /// Certificates from an ACME CA (Let's Encrypt), cached on disk.
    Acme { domains: Vec<String>, email: Option<String>, cache_dir: PathBuf, staging: bool },
}

#[derive(Debug, Clone)]
pub struct TlsConfig {
    pub bind: SocketAddr,
    pub source: CertSource,
}

/// Serves `app` over HTTPS until `shutdown` resolves, then drains.
pub async fn serve(
    cfg: TlsConfig,
    app: axum::Router,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let _ = (cfg, app, shutdown);
    Err(std::io::Error::other("TLS not implemented yet"))
}
