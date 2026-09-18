//! `CertSource::Acme`: TLS-ALPN-01 certificates from an ACME CA (Let's
//! Encrypt), via `rustls-acme`. **Untested against a real CA** — see
//! NOTES.md. Every non-challenge connection still advertises h2 then
//! http/1.1, same as the `Files` path.

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use rustls::ServerConfig;
use rustls::server::Acceptor;
use rustls_acme::caches::DirCache;
use rustls_acme::{AcmeConfig, is_tls_alpn_challenge};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio_rustls::LazyConfigAcceptor;
use tokio_rustls::server::TlsStream;
use tokio_stream::StreamExt;

use crate::accept::{self, Handshake};

pub(crate) async fn serve_acme(
    bind: SocketAddr,
    domains: Vec<String>,
    email: Option<String>,
    cache_dir: PathBuf,
    staging: bool,
    app: axum::Router,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    if domains.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "acme: at least one domain is required"));
    }

    let mut config = AcmeConfig::new(domains).cache(DirCache::new(cache_dir)).directory_lets_encrypt(!staging);
    if let Some(email) = &email {
        config = config.contact([format!("mailto:{email}")]);
    }
    let mut state = config.state();

    // `default_rustls_config()` builds a ServerConfig around the ACME
    // resolver with no ALPN set; clone it and set our own so normal
    // connections still negotiate h2/http1.1 the same as the Files path.
    // The challenge config is used as-is: rustls-acme already restricts it
    // to the "acme-tls/1" ALPN identifier required by TLS-ALPN-01.
    let mut default_config: ServerConfig = (*state.default_rustls_config()).clone();
    default_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let default_config = Arc::new(default_config);
    let challenge_config = state.challenge_rustls_config();

    // Drives the ACME order/renewal state machine. Runs for the process
    // lifetime; errors (rate limits, a failed challenge, cache I/O) are
    // logged and never stop the HTTP accept loop below.
    tokio::spawn(async move {
        while let Some(event) = state.next().await {
            match event {
                Ok(ok) => tracing::info!(event = ?ok, "acme event"),
                Err(err) => tracing::error!(error = %err, "acme error"),
            }
        }
    });

    accept::run(bind, AcmeHandshake { default_config, challenge_config }, app, shutdown).await
}

struct AcmeHandshake {
    default_config: Arc<ServerConfig>,
    challenge_config: Arc<ServerConfig>,
}

impl Handshake for AcmeHandshake {
    fn accept(
        &self,
        tcp: TcpStream,
    ) -> impl std::future::Future<Output = io::Result<Option<TlsStream<TcpStream>>>> + Send {
        let default_config = self.default_config.clone();
        let challenge_config = self.challenge_config.clone();
        async move {
            let start = LazyConfigAcceptor::new(Acceptor::default(), tcp).await?;
            if is_tls_alpn_challenge(&start.client_hello()) {
                let mut tls = start.into_stream(challenge_config).await?;
                // The challenge is satisfied by presenting the special
                // certificate during the handshake; there's no HTTP request
                // to serve on this connection.
                let _ = tls.shutdown().await;
                Ok(None)
            } else {
                let stream = start.into_stream(default_config).await?;
                Ok(Some(stream))
            }
        }
    }
}
