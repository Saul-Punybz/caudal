//! HTTPS for Caudal: HTTP/1.1 and HTTP/2 negotiated by ALPN, certificates
//! from files (reloaded when they change) or from Let's Encrypt (ACME).
//! Entry point fixed by the orchestrator.
//!
//! Crypto provider: `ring`, not `aws-lc-rs` — it keeps the static musl
//! build simple (no cc/cmake toolchain needed to cross-compile it), and
//! Caudal doesn't need FIPS or the aws-lc-rs performance edge here.

use std::net::SocketAddr;
use std::path::PathBuf;

mod accept;
mod acme;
mod files;
mod resolver;

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
///
/// Each connection speaks HTTP/1.1 or HTTP/2 depending on what ALPN
/// negotiates (`h2` offered before `http/1.1`), via `hyper-util`'s `auto`
/// builder. `axum::extract::ConnectInfo<SocketAddr>` is inserted into every
/// request's extensions, same as `axum::serve(..).into_make_service_with_connect_info`
/// does for the plain-HTTP listener in `crates/caudal/src/main.rs`.
pub async fn serve(
    cfg: TlsConfig,
    app: axum::Router,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    // Installed once per process; ignored if already installed (e.g. a
    // second `serve()` call, or another crate installed one first).
    let _ = rustls::crypto::ring::default_provider().install_default();

    match cfg.source {
        CertSource::Files { cert, key } => files::serve_files(cfg.bind, cert, key, app, shutdown).await,
        CertSource::Acme { domains, email, cache_dir, staging } => {
            acme::serve_acme(cfg.bind, domains, email, cache_dir, staging, app, shutdown).await
        }
    }
}
