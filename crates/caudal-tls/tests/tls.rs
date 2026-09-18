//! Integration tests for `caudal-tls`'s `CertSource::Files` path: ALPN
//! negotiation (h2 with an h1 fallback), hot certificate reload, and
//! graceful shutdown. `CertSource::Acme` is not covered here — it needs a
//! public domain and a real ACME CA; see NOTES.md.
//!
//! Run one at a time, per the batch's machine rule:
//! `cargo test -p caudal-tls -- --test-threads=1`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::routing::get;
use caudal_tls::{CertSource, TlsConfig};
use rcgen::CertifiedKey;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

fn test_app() -> Router {
    Router::new().route("/ping", get(|| async { "pong" }))
}

/// Writes a freshly generated self-signed cert/key pair for `sans` to
/// `<dir>/<name>.{cert,key}.pem`, overwriting any previous cert at the same
/// path (used to exercise hot reload).
fn write_cert(dir: &Path, name: &str, sans: &[&str]) -> (PathBuf, PathBuf, CertifiedKey) {
    let sans: Vec<String> = sans.iter().map(|s| s.to_string()).collect();
    let certified = rcgen::generate_simple_self_signed(sans).expect("generate self-signed cert");
    let cert_path = dir.join(format!("{name}.cert.pem"));
    let key_path = dir.join(format!("{name}.key.pem"));
    std::fs::write(&cert_path, certified.cert.pem()).expect("write cert pem");
    std::fs::write(&key_path, certified.key_pair.serialize_pem()).expect("write key pem");
    (cert_path, key_path, certified)
}

async fn free_addr() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
    listener.local_addr().expect("local_addr")
}

fn spawn_server(
    bind: SocketAddr,
    source: CertSource,
    app: Router,
) -> (oneshot::Sender<()>, tokio::task::JoinHandle<std::io::Result<()>>) {
    let (tx, rx) = oneshot::channel();
    let cfg = TlsConfig { bind, source };
    let handle = tokio::spawn(async move {
        caudal_tls::serve(cfg, app, async {
            let _ = rx.await;
        })
        .await
    });
    (tx, handle)
}

/// Polls with a raw TCP connect until the listener is accepting, since
/// `serve()`'s bind happens asynchronously after `spawn_server` returns.
async fn wait_until_listening(addr: SocketAddr) {
    for _ in 0..100 {
        if TcpStream::connect(addr).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("server never started listening on {addr}");
}

/// A `ServerCertVerifier` that accepts any certificate (this is a test
/// against a self-signed cert with no real CA) and records the leaf
/// certificate DER it was shown, so the test can tell which cert a
/// connection actually got.
#[derive(Debug)]
struct CapturingVerifier {
    provider: Arc<rustls::crypto::CryptoProvider>,
    captured: Arc<Mutex<Option<Vec<u8>>>>,
}

impl ServerCertVerifier for CapturingVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        *self.captured.lock().expect("lock") = Some(end_entity.as_ref().to_vec());
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// Connects with a bare rustls client (no HTTP involved) and returns the
/// DER bytes of whatever leaf certificate the server presents, so a test
/// can compare it against a known cert's own DER.
async fn fetch_leaf_cert(addr: SocketAddr) -> Vec<u8> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let captured = Arc::new(Mutex::new(None));
    let verifier = Arc::new(CapturingVerifier { provider: provider.clone(), captured: captured.clone() });
    let client_config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
    let tcp = TcpStream::connect(addr).await.expect("tcp connect");
    let server_name = ServerName::try_from("localhost").expect("server name");
    let tls = connector.connect(server_name, tcp).await.expect("tls connect");
    drop(tls);
    captured.lock().expect("lock").take().expect("verifier was called")
}

#[tokio::test]
async fn serves_h2_over_alpn_with_h1_fallback() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (cert_path, key_path, _cert) = write_cert(dir.path(), "a", &["localhost"]);
    let addr = free_addr().await;
    let (shutdown_tx, handle) = spawn_server(addr, CertSource::Files { cert: cert_path, key: key_path }, test_app());
    wait_until_listening(addr).await;

    // (a) a default client (ALPN offers h2) gets HTTP/2.
    let h2_client = reqwest::Client::builder().danger_accept_invalid_certs(true).build().expect("h2 client");
    let resp = h2_client.get(format!("https://{addr}/ping")).send().await.expect("h2 request");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert_eq!(resp.version(), reqwest::Version::HTTP_2, "expected ALPN to negotiate h2");
    assert_eq!(resp.text().await.expect("h2 body"), "pong");

    // (b) an HTTP/1.1-only client still works over the same listener.
    let h1_client =
        reqwest::Client::builder().danger_accept_invalid_certs(true).http1_only().build().expect("h1 client");
    let resp = h1_client.get(format!("https://{addr}/ping")).send().await.expect("h1 request");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert_eq!(resp.version(), reqwest::Version::HTTP_11);
    assert_eq!(resp.text().await.expect("h1 body"), "pong");

    drop(h2_client);
    drop(h1_client);

    // Cross-check with curl if it's on PATH; expect http_version "2". Run on
    // a blocking-pool thread: `Command::output()` blocks the calling OS
    // thread, and this test's runtime is single-threaded, so calling it
    // inline would starve the very server curl is trying to reach.
    let url = format!("https://{addr}/ping");
    let curl_result = tokio::task::spawn_blocking(move || {
        std::process::Command::new("curl")
            .args(["-sk", "--http2", "-m", "5", "-o", "/dev/null", "-w", "%{http_version}", &url])
            .output()
    })
    .await
    .expect("curl task did not panic");
    match curl_result {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout).into_owned();
            eprintln!("curl --http2 -k reported http_version={version}");
            assert_eq!(version, "2", "curl did not negotiate HTTP/2");
        }
        Ok(output) => eprintln!("curl exited with {:?}, skipping the cross-check", output.status),
        Err(e) => eprintln!("curl not available ({e}), skipping the cross-check"),
    }

    let _ = shutdown_tx.send(());
    tokio::time::timeout(Duration::from_secs(6), handle)
        .await
        .expect("serve() returned within 6s of shutdown")
        .expect("server task did not panic")
        .expect("serve() exited cleanly");
}

#[tokio::test]
async fn hot_reloads_certificate_without_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (cert_path, key_path, cert1) = write_cert(dir.path(), "a", &["localhost"]);
    let addr = free_addr().await;
    let (shutdown_tx, handle) =
        spawn_server(addr, CertSource::Files { cert: cert_path.clone(), key: key_path.clone() }, test_app());
    wait_until_listening(addr).await;

    let leaf_before = fetch_leaf_cert(addr).await;
    assert_eq!(leaf_before, cert1.cert.der().as_ref(), "server did not present the initial certificate");

    // Give the filesystem clock room to move forward before overwriting,
    // so the reloader's mtime poll actually sees a change.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let (_cert_path2, _key_path2, cert2) = write_cert(dir.path(), "a", &["localhost"]);

    // The reloader polls every 5s; give it up to 8s across a few tries.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    let mut leaf_after = leaf_before;
    while tokio::time::Instant::now() < deadline {
        leaf_after = fetch_leaf_cert(addr).await;
        if leaf_after == cert2.cert.der().as_ref() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    assert_eq!(leaf_after, cert2.cert.der().as_ref(), "server did not pick up the reloaded certificate in time");

    let _ = shutdown_tx.send(());
    tokio::time::timeout(Duration::from_secs(6), handle)
        .await
        .expect("serve() returned within 6s of shutdown")
        .expect("server task did not panic")
        .expect("serve() exited cleanly");
}

#[tokio::test]
async fn graceful_shutdown_returns_within_cap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (cert_path, key_path, _cert) = write_cert(dir.path(), "a", &["localhost"]);
    let addr = free_addr().await;
    let (shutdown_tx, handle) = spawn_server(addr, CertSource::Files { cert: cert_path, key: key_path }, test_app());
    wait_until_listening(addr).await;

    let _ = shutdown_tx.send(());
    let result = tokio::time::timeout(Duration::from_secs(5), handle).await;
    assert!(result.is_ok(), "serve() did not return within the 5s graceful shutdown cap");
    result.expect("timeout").expect("server task did not panic").expect("serve() exited cleanly");
}
