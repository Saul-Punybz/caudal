//! The accept loop shared by both `CertSource` backends: accept a TCP
//! connection, run the TLS handshake (details differ between plain files
//! and ACME, see [`Handshake`]), then hand the connection to hyper-util's
//! `auto` builder so it speaks HTTP/1.1 or HTTP/2 per the negotiated ALPN.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Extension;
use axum::extract::ConnectInfo;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as AutoBuilder;
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::server::TlsStream;

/// Per-connection TLS handshake, abstracting over `CertSource::Files`
/// (a plain `TlsAcceptor`) and `CertSource::Acme` (the TLS-ALPN-01
/// challenge/normal-connection split). Returns `Ok(None)` for a connection
/// that was fully handled by the handshake itself and has nothing left to
/// serve over HTTP (an ACME challenge probe).
pub(crate) trait Handshake: Send + Sync + 'static {
    fn accept(&self, tcp: TcpStream) -> impl Future<Output = io::Result<Option<TlsStream<TcpStream>>>> + Send;
}

/// TCP accept timeout for the TLS handshake. Protects against scanners and
/// slow-loris style connections that never complete a handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Cap on how long graceful shutdown waits for in-flight connections to
/// finish draining before giving up and returning anyway.
const GRACEFUL_SHUTDOWN_CAP: Duration = Duration::from_secs(5);

/// Runs the accept loop on `bind` until `shutdown` resolves, then stops
/// accepting new connections and drains in-flight ones (capped at 5s).
pub(crate) async fn run<H>(
    bind: SocketAddr,
    handshake: H,
    app: axum::Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()>
where
    H: Handshake,
{
    let listener = TcpListener::bind(bind).await?;
    tracing::info!(bind = %bind, "tls listener started");

    let handshake = Arc::new(handshake);
    // ALPN order is set by the caller on the rustls ServerConfig(s); this
    // builder just speaks whichever of h1/h2 the handshake negotiated.
    let builder = Arc::new(AutoBuilder::new(TokioExecutor::new()));
    let graceful = GracefulShutdown::new();
    let mut shutdown = std::pin::pin!(shutdown);

    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => break,
            accepted = listener.accept() => {
                let (tcp, peer_addr) = match accepted {
                    Ok(pair) => pair,
                    Err(e) => {
                        tracing::debug!(error = %e, "tcp accept error");
                        continue;
                    }
                };
                spawn_connection(tcp, peer_addr, handshake.clone(), app.clone(), builder.clone(), graceful.watcher());
            }
        }
    }

    // Stop accepting; let what's in flight finish, capped at 5s.
    drop(listener);
    tokio::select! {
        _ = graceful.shutdown() => {
            tracing::info!("tls connections drained");
        }
        _ = tokio::time::sleep(GRACEFUL_SHUTDOWN_CAP) => {
            tracing::warn!("graceful shutdown timed out after {:?}, dropping remaining connections", GRACEFUL_SHUTDOWN_CAP);
        }
    }

    Ok(())
}

fn spawn_connection<H>(
    tcp: TcpStream,
    peer_addr: SocketAddr,
    handshake: Arc<H>,
    app: axum::Router,
    builder: Arc<AutoBuilder<TokioExecutor>>,
    watcher: hyper_util::server::graceful::Watcher,
) where
    H: Handshake,
{
    tokio::spawn(async move {
        let tls = match tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake.accept(tcp)).await {
            Ok(Ok(Some(tls))) => tls,
            // Handled entirely inside the handshake (e.g. an ACME TLS-ALPN-01
            // challenge probe); nothing left to serve over HTTP.
            Ok(Ok(None)) => return,
            Ok(Err(e)) => {
                tracing::debug!(error = %e, peer = %peer_addr, "tls handshake failed");
                return;
            }
            Err(_) => {
                tracing::debug!(peer = %peer_addr, "tls handshake timed out");
                return;
            }
        };

        // Matches `into_make_service_with_connect_info`: one extension
        // insertion per connection, not per request. caudal-hls reads it
        // via `axum::extract::ConnectInfo<SocketAddr>` to count viewers.
        let svc = app.layer(Extension(ConnectInfo(peer_addr)));
        let io = TokioIo::new(tls);
        let hyper_svc = TowerToHyperService::new(svc);
        let conn = builder.serve_connection_with_upgrades(io, hyper_svc);
        let conn = watcher.watch(conn.into_owned());

        if let Err(err) = conn.await {
            tracing::debug!(error = %err, peer = %peer_addr, "connection error");
        }
    });
}
