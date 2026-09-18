//! Media over QUIC output: every live stream in the registry is published as
//! a `hang` broadcast named after the stream, served over QUIC/WebTransport
//! by an embedded moq-native server. Entry point fixed by the orchestrator.
//!
//! The moq-dev crates move fast; they are pinned to exact versions.
//!
//! - One local origin. [`publish`] turns each registry stream into a
//!   broadcast (catalog + one Legacy-container track per codec track).
//! - [`session`] accepts browser WebTransport sessions and native QUIC
//!   clients, checks `?jwt=` against the registry's gate, and counts viewers.
//! - [`cert`] generates and rotates the self-signed certificate browsers pin
//!   through `serverCertificateHashes`.

mod cert;
mod publish;
mod session;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::IntoResponse;
use axum::routing::get;
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

/// The running MoQ output. The server, publishers and certificate rotation
/// run as detached tasks; dropping this handle does not stop them.
pub struct MoqService {
    info: Arc<EndpointInfo>,
}

/// What `/moq/fingerprint` reports.
struct EndpointInfo {
    /// Hostname for the URL when the request carries no `Host` header.
    default_host: String,
    port: u16,
    /// Live view of the served certificates; `None` for file certificates,
    /// which browsers verify through the WebPKI instead of a pinned hash.
    pinned: Option<moq_native::tls::Certificates>,
}

impl EndpointInfo {
    fn json(&self, request_host: Option<&str>) -> serde_json::Value {
        let host = request_host.map(hostname_of).filter(|h| !h.is_empty()).unwrap_or(&self.default_host);
        let fingerprint = self.pinned.as_ref().and_then(|c| c.fingerprints().into_iter().next());
        serde_json::json!({
            "url": format!("https://{host}:{}", self.port),
            "fingerprint": fingerprint.map(|f| f.to_ascii_lowercase()),
        })
    }
}

/// `example.com:8080` → `example.com`, `[::1]:8080` → `[::1]`.
fn hostname_of(authority: &str) -> &str {
    if authority.starts_with('[') {
        return match authority.find(']') {
            Some(end) => &authority[..=end],
            None => authority,
        };
    }
    authority.rsplit_once(':').map_or(authority, |(h, _)| h)
}

async fn fingerprint(State(info): State<Arc<EndpointInfo>>, headers: HeaderMap) -> impl IntoResponse {
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok());
    let body = info.json(host).to_string();
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("application/json")),
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*")),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        body,
    )
}

impl MoqService {
    /// Serves `GET /moq/fingerprint` (JSON `{"url": "https://host:port",
    /// "fingerprint": "<sha-256 hex>" | null}`) so players can connect to a
    /// self-signed endpoint. Merge into the HTTP app.
    ///
    /// `host` is the hostname of the request's `Host` header (the name the
    /// browser already reached Caudal by), falling back to the first
    /// configured host; the port is the QUIC port actually bound.
    pub fn router(&self) -> axum::Router {
        axum::Router::new().route("/moq/fingerprint", get(fingerprint)).with_state(self.info.clone())
    }
}

/// Binds QUIC and starts publishing every current and future stream. Must
/// be called inside a tokio runtime.
pub fn start(registry: Arc<Registry>, cfg: MoqConfig) -> std::io::Result<MoqService> {
    let (tls, default_host, rotation) = match &cfg.cert {
        MoqCert::Files { cert, key } => {
            let mut tls = moq_native::tls::Server::default();
            tls.cert = vec![cert.clone()];
            tls.key = vec![key.clone()];
            (tls, "localhost".to_owned(), None)
        }
        MoqCert::SelfSigned { hosts } => {
            let hosts = if hosts.is_empty() { vec!["localhost".to_owned()] } else { hosts.clone() };
            let rotating = cert::SelfSigned::create(hosts.clone())?;
            let mut tls = moq_native::tls::Server::default();
            tls.cert = vec![rotating.cert_path()];
            tls.key = vec![rotating.key_path()];
            (tls, hosts[0].clone(), Some(rotating))
        }
    };

    let mut server_cfg = moq_native::ServerConfig::default();
    server_cfg.bind = Some(cfg.bind.to_string());
    server_cfg.tls = tls;
    let server = server_cfg.init().map_err(std::io::Error::other)?;
    let port = server.local_addr().map_err(std::io::Error::other)?.port();
    let certs = server.certificates();

    let origin = moq_net::Origin::random().produce();
    let stats = moq_net::stats::Registry::new(moq_net::stats::Config::new());

    publish::spawn_all(registry.clone(), origin.clone());
    session::spawn_viewer_counts(registry.clone(), stats.clone());
    session::spawn_accept(server, registry, origin, stats);

    let pinned = rotation.map(|r| {
        r.spawn_rotation();
        certs
    });
    Ok(MoqService { info: Arc::new(EndpointInfo { default_host, port, pinned }) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[test]
    fn hostnames() {
        assert_eq!(hostname_of("example.com:8080"), "example.com");
        assert_eq!(hostname_of("example.com"), "example.com");
        assert_eq!(hostname_of("[::1]:8080"), "[::1]");
        assert_eq!(hostname_of("127.0.0.1:80"), "127.0.0.1");
    }

    async fn get_json(svc: &MoqService, host: Option<&str>) -> (axum::http::HeaderMap, serde_json::Value) {
        let mut req = Request::get("/moq/fingerprint");
        if let Some(h) = host {
            req = req.header("host", h);
        }
        let res = svc.router().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(res.status(), 200);
        let headers = res.headers().clone();
        let body = axum::body::to_bytes(res.into_body(), 1 << 16).await.unwrap();
        (headers, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn fingerprint_json_self_signed() {
        let svc = start(
            Registry::new(),
            MoqConfig {
                bind: "127.0.0.1:0".parse().unwrap(),
                cert: MoqCert::SelfSigned { hosts: vec!["caudal.test".into(), "127.0.0.1".into()] },
            },
        )
        .unwrap();
        let port = svc.info.port;
        assert_ne!(port, 0);

        let (headers, v) = get_json(&svc, None).await;
        assert_eq!(headers["access-control-allow-origin"], "*");
        assert_eq!(headers["content-type"], "application/json");
        assert_eq!(v["url"], format!("https://caudal.test:{port}"));
        let fp = v["fingerprint"].as_str().expect("fingerprint string");
        assert_eq!(fp.len(), 64);
        assert!(fp.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)), "{fp}");

        let (_, v) = get_json(&svc, Some("10.0.0.5:8080")).await;
        assert_eq!(v["url"], format!("https://10.0.0.5:{port}"));
        assert_eq!(v["fingerprint"], fp);
    }

    #[tokio::test]
    async fn fingerprint_json_files_is_null() {
        let dir = tempfile::tempdir().unwrap();
        let generated = cert::generate(&["localhost".to_owned()]).unwrap();
        let (cert, key) = (dir.path().join("c.pem"), dir.path().join("k.pem"));
        std::fs::write(&cert, &generated.cert_pem).unwrap();
        std::fs::write(&key, &generated.key_pem).unwrap();
        let svc = start(Registry::new(), MoqConfig { bind: "127.0.0.1:0".parse().unwrap(), cert: MoqCert::Files { cert, key } })
            .unwrap();
        let (_, v) = get_json(&svc, Some("media.example.org")).await;
        assert_eq!(v["url"], format!("https://media.example.org:{}", svc.info.port));
        assert!(v["fingerprint"].is_null());
    }

    #[tokio::test]
    async fn bad_cert_files_are_an_error() {
        let r = start(
            Registry::new(),
            MoqConfig {
                bind: "127.0.0.1:0".parse().unwrap(),
                cert: MoqCert::Files { cert: "/nonexistent/c.pem".into(), key: "/nonexistent/k.pem".into() },
            },
        );
        assert!(r.is_err());
    }
}
