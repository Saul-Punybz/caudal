//! `CertSource::Files`: TLS from a hot-reloaded PEM cert/key pair.

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::net::TcpStream;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

use crate::accept::{self, Handshake};

/// h2 first, http/1.1 second: browsers and curl pick the first mutually
/// supported protocol, and Apple's HLS validator requires h2 to be offered.
fn alpn_protocols() -> Vec<Vec<u8>> {
    vec![b"h2".to_vec(), b"http/1.1".to_vec()]
}

pub(crate) async fn serve_files(
    bind: SocketAddr,
    cert: PathBuf,
    key: PathBuf,
    app: axum::Router,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    let resolver = crate::resolver::FileCertResolver::load(cert, key)?;
    resolver.spawn_reloader();

    let mut server_config =
        rustls::ServerConfig::builder().with_no_client_auth().with_cert_resolver(resolver);
    server_config.alpn_protocols = alpn_protocols();

    let acceptor = TlsAcceptor::from(Arc::new(server_config));
    accept::run(bind, FilesHandshake(acceptor), app, shutdown).await
}

struct FilesHandshake(TlsAcceptor);

impl Handshake for FilesHandshake {
    fn accept(
        &self,
        tcp: TcpStream,
    ) -> impl std::future::Future<Output = io::Result<Option<TlsStream<TcpStream>>>> + Send {
        let acceptor = self.0.clone();
        async move { acceptor.accept(tcp).await.map(Some) }
    }
}
