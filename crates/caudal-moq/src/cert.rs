//! The self-signed certificate browsers pin with WebTransport
//! `serverCertificateHashes`: ECDSA P-256, valid for less than 14 days, and
//! replaced before it expires.
//!
//! moq-native only serves certificates from files (or one it generates and
//! never renews), but it watches those files and hot-swaps them for new
//! handshakes. So the certificate lives as PEM files in a private temporary
//! directory and rotation is "write new files, rename into place": new
//! connections get the new certificate, open sessions keep theirs, and the
//! endpoint never restarts. `/moq/fingerprint` reads the live fingerprint
//! from moq-native, so it always names the certificate actually served.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Total validity. Chrome rejects pinned certificates valid for longer than
/// 14 days; stay a day under.
pub(crate) const VALIDITY: Duration = Duration::from_secs(13 * 24 * 3600);
/// Backdating, for clients whose clock runs a little behind ours.
const BACKDATE: Duration = Duration::from_secs(3600);
/// How often a fresh certificate replaces the current one: well before
/// expiry, so a player that fetched the fingerprint just before a rotation
/// still connects (both certificates stay valid for days).
pub(crate) const ROTATE_EVERY: Duration = Duration::from_secs(6 * 24 * 3600);

pub(crate) struct Generated {
    pub cert_pem: String,
    pub key_pem: String,
    /// Lowercase hex SHA-256 of the DER leaf.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fingerprint: String,
}

/// `(not_before, not_after)` for a certificate issued at `now`.
fn validity(now: time::OffsetDateTime) -> (time::OffsetDateTime, time::OffsetDateTime) {
    let not_before = now - BACKDATE;
    (not_before, not_before + VALIDITY)
}

pub(crate) fn generate(hosts: &[String]) -> io::Result<Generated> {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).map_err(io::Error::other)?;
    let mut params = rcgen::CertificateParams::new(hosts.to_vec()).map_err(io::Error::other)?;
    (params.not_before, params.not_after) = validity(time::OffsetDateTime::now_utc());
    params.distinguished_name.push(rcgen::DnType::CommonName, "Caudal MoQ");
    let cert = params.self_signed(&key).map_err(io::Error::other)?;
    let digest = ring::digest::digest(&ring::digest::SHA256, cert.der());
    let fingerprint = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    Ok(Generated { cert_pem: cert.pem(), key_pem: key.serialize_pem(), fingerprint })
}

/// A rotating self-signed certificate on disk.
pub(crate) struct SelfSigned {
    hosts: Vec<String>,
    dir: tempfile::TempDir,
}

impl SelfSigned {
    pub fn create(hosts: Vec<String>) -> io::Result<Self> {
        // `tempdir` creates the directory 0700: the key never leaves this user.
        let dir = tempfile::Builder::new().prefix("caudal-moq-").tempdir()?;
        let this = Self { hosts, dir };
        this.rotate()?;
        Ok(this)
    }

    pub fn cert_path(&self) -> PathBuf {
        self.dir.path().join("cert.pem")
    }

    pub fn key_path(&self) -> PathBuf {
        self.dir.path().join("key.pem")
    }

    /// Writes a new certificate and key, each renamed into place so a reader
    /// never sees a half-written file. moq-native's watcher may fire between
    /// the two renames; that load fails the key check, keeps the previous
    /// pair and the second rename reloads the matching one.
    pub fn rotate(&self) -> io::Result<Generated> {
        let g = generate(&self.hosts)?;
        write_atomic(&self.key_path(), g.key_pem.as_bytes())?;
        write_atomic(&self.cert_path(), g.cert_pem.as_bytes())?;
        Ok(g)
    }

    /// Rotates every [`ROTATE_EVERY`] for the life of the process.
    pub fn spawn_rotation(self) {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(ROTATE_EVERY).await;
                match self.rotate() {
                    Ok(g) => tracing::info!(fingerprint = %g.fingerprint, "moq: rotated self-signed certificate"),
                    Err(e) => tracing::error!(error = %e, "moq: certificate rotation failed; retrying in 1 h"),
                }
                // A failed rotation retries sooner; a good one waits the full period
                // at the top of the loop again (the extra hour is harmless).
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        });
    }
}

fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validity_is_under_fourteen_days_and_covers_now() {
        let now = time::OffsetDateTime::now_utc();
        let (nb, na) = validity(now);
        assert!(nb < now && now < na);
        assert!(na - nb < time::Duration::days(14));
        assert!(ROTATE_EVERY < VALIDITY);
    }

    #[test]
    fn generates_ecdsa_p256_pem() {
        let g = generate(&["localhost".into(), "127.0.0.1".into()]).unwrap();
        assert!(g.cert_pem.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(g.key_pem.contains("PRIVATE KEY"));
        assert_eq!(g.fingerprint.len(), 64);
        let key = rcgen::KeyPair::from_pem(&g.key_pem).unwrap();
        assert!(key.is_compatible(&rcgen::PKCS_ECDSA_P256_SHA256));
    }

    /// A rotation reaches a running moq-native server without a restart.
    #[tokio::test]
    async fn rotation_is_picked_up_live() {
        let ss = SelfSigned::create(vec!["localhost".into()]).unwrap();
        let mut cfg = moq_native::ServerConfig::default();
        cfg.bind = Some("127.0.0.1:0".into());
        cfg.tls.cert = vec![ss.cert_path()];
        cfg.tls.key = vec![ss.key_path()];
        let server = cfg.init().unwrap();
        let certs = server.certificates();
        let before = certs.fingerprints();
        assert_eq!(before.len(), 1);

        // The watcher is spawned with the server; give it a moment to arm.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let g = ss.rotate().unwrap();
        assert_ne!(before[0], g.fingerprint);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while certs.fingerprints() != vec![g.fingerprint.clone()] {
            assert!(tokio::time::Instant::now() < deadline, "rotated certificate never loaded");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        drop(server);
    }
}
