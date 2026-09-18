//! `CertSource::Files`: a certificate resolver backed by PEM files on disk,
//! reloaded when either file's mtime changes.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use arc_swap::ArcSwap;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};

/// How often the background task checks the cert/key files for changes.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Resolves the TLS certificate from PEM files, hot-reloading them without a
/// restart. A background task (spawned by [`FileCertResolver::spawn_reloader`])
/// polls both files' mtimes every 5s; when either changes it re-reads and
/// re-parses them and swaps in the new certified key. A reload that fails
/// (missing file, bad PEM, mismatched key) is logged and the previous,
/// still-valid certificate keeps serving.
#[derive(Debug)]
pub(crate) struct FileCertResolver {
    cert_path: PathBuf,
    key_path: PathBuf,
    current: ArcSwap<CertifiedKey>,
}

impl FileCertResolver {
    /// Loads the initial certificate. Fails the whole `serve()` call if the
    /// files are missing or invalid at startup.
    pub(crate) fn load(cert_path: PathBuf, key_path: PathBuf) -> io::Result<Arc<Self>> {
        let key = load_certified_key(&cert_path, &key_path)?;
        Ok(Arc::new(Self { cert_path, key_path, current: ArcSwap::from_pointee(key) }))
    }

    /// Spawns the polling reload task. Runs for the lifetime of the process;
    /// there is nothing to cancel it, which is fine since it is cheap and
    /// harmless to keep polling after `serve()`'s accept loop stops.
    pub(crate) fn spawn_reloader(self: &Arc<Self>) {
        let resolver = self.clone();
        tokio::spawn(async move {
            let mut last = file_mtimes(&resolver.cert_path, &resolver.key_path);
            loop {
                tokio::time::sleep(POLL_INTERVAL).await;
                let now = file_mtimes(&resolver.cert_path, &resolver.key_path);
                if now == last {
                    continue;
                }
                // Always advance `last`, even on a failed reload: this
                // avoids hammering a persistently-bad file every 5s, at the
                // cost of needing one more change to trigger a retry.
                last = now;
                match load_certified_key(&resolver.cert_path, &resolver.key_path) {
                    Ok(key) => {
                        resolver.current.store(Arc::new(key));
                        tracing::info!(
                            cert = %resolver.cert_path.display(),
                            "tls certificate reloaded"
                        );
                    }
                    Err(e) => {
                        tracing::error!(
                            error = %e,
                            cert = %resolver.cert_path.display(),
                            "tls certificate reload failed, keeping previous certificate"
                        );
                    }
                }
            }
        });
    }
}

impl ResolvesServerCert for FileCertResolver {
    fn resolve(&self, _client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.current.load_full())
    }
}

fn file_mtimes(cert: &Path, key: &Path) -> (Option<SystemTime>, Option<SystemTime>) {
    (mtime(cert), mtime(key))
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn load_certified_key(cert_path: &Path, key_path: &Path) -> io::Result<CertifiedKey> {
    let cert_bytes = std::fs::read(cert_path)
        .map_err(|e| io::Error::new(e.kind(), format!("reading {}: {e}", cert_path.display())))?;
    let key_bytes = std::fs::read(key_path)
        .map_err(|e| io::Error::new(e.kind(), format!("reading {}: {e}", key_path.display())))?;

    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_bytes.as_slice())
        .collect::<Result<_, _>>()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("parsing {}: {e}", cert_path.display())))?;
    if certs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} contains no certificates", cert_path.display()),
        ));
    }

    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut key_bytes.as_slice())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("parsing {}: {e}", key_path.display())))?
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, format!("{} contains no private key", key_path.display()))
        })?;

    let provider = rustls::crypto::ring::default_provider();
    CertifiedKey::from_der(certs, key, &provider)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("loading key/cert from {}: {e}", key_path.display())))
}
