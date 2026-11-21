//! HTTPS for Caudal: HTTP/1.1 and HTTP/2 negotiated by ALPN, certificates
//! from files (reloaded when they change) or from Let's Encrypt (ACME).
//! Entry point fixed by the orchestrator.
//!
//! Crypto provider: `ring`, not `aws-lc-rs` — it keeps the static musl
//! build simple (no cc/cmake toolchain needed to cross-compile it), and
//! Caudal doesn't need FIPS or the aws-lc-rs performance edge here.

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

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

/// A bare rustls TLS acceptor for protocols other than HTTP: no ALPN
/// negotiation, no axum `Router`, just "TLS handshake in, `AsyncRead +
/// AsyncWrite` stream out". Used by `caudal-rtsp` for RTSPS (RTSP text and
/// interleaved RTP/RTCP framed inside the TLS record layer, same as media
/// over plain TCP interleaved). Certificates hot-reload the same way
/// `CertSource::Files` does for HTTPS.
pub async fn raw_tls_acceptor(cert: PathBuf, key: PathBuf) -> io::Result<tokio_rustls::TlsAcceptor> {
    tls_acceptor(&CertSource::Files { cert, key }).await
}

/// [`raw_tls_acceptor`] for any [`CertSource`], so a non-HTTP listener
/// (RTMPS) can present the same certificate as the HTTPS listener.
///
/// - `Files`: its own hot-reloaded copy of the PEM pair (polled every 5 s,
///   same as HTTPS; the poller stops when the acceptor is dropped).
/// - `Acme`: no second ACME client. The acceptor resolves through the
///   certificate the running HTTPS listener ([`serve`]) orders and renews,
///   so renewals are picked up without a restart. That listener is also
///   where TLS-ALPN-01 challenges land, so `[tls]` must be serving for an
///   ACME certificate to exist; until the first one is issued (or if the
///   HTTPS listener isn't running) handshakes fail with no certificate.
pub async fn tls_acceptor(source: &CertSource) -> io::Result<tokio_rustls::TlsAcceptor> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let resolver: Arc<dyn rustls::server::ResolvesServerCert> = match source {
        CertSource::Files { cert, key } => {
            let resolver = resolver::FileCertResolver::load(cert.clone(), key.clone())?;
            resolver.spawn_reloader();
            resolver
        }
        CertSource::Acme { .. } => Arc::new(acme::SharedAcmeResolver),
    };
    let server_config = rustls::ServerConfig::builder().with_no_client_auth().with_cert_resolver(resolver);
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(server_config)))
}
