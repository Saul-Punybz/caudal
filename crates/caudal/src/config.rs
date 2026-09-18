//! TOML configuration: every section and key optional with fixed defaults,
//! unknown keys rejected so a typo fails loudly instead of silently no-op-ing.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use caudal_core::BufferConfig;
use serde::Deserialize;

fn default_http_bind() -> SocketAddr {
    "0.0.0.0:8080".parse().unwrap()
}

fn default_rtmp_bind() -> SocketAddr {
    "0.0.0.0:1935".parse().unwrap()
}

fn default_rtmp_app() -> String {
    "live".to_string()
}

fn default_srt_bind() -> SocketAddr {
    "0.0.0.0:9000".parse().unwrap()
}

fn default_srt_latency_ms() -> u32 {
    120
}

fn default_part_ms() -> u32 {
    200
}

fn default_segment_ms() -> u32 {
    2000
}

fn default_window_secs() -> u64 {
    50
}

fn default_max_mb() -> usize {
    256
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct ServerSection {
    #[serde(default = "default_http_bind")]
    pub http_bind: SocketAddr,
}

impl Default for ServerSection {
    fn default() -> Self {
        Self { http_bind: default_http_bind() }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct RtmpSection {
    #[serde(default = "default_rtmp_bind")]
    pub bind: SocketAddr,
    #[serde(default = "default_rtmp_app")]
    pub app: String,
}

impl Default for RtmpSection {
    fn default() -> Self {
        Self { bind: default_rtmp_bind(), app: default_rtmp_app() }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct SrtSection {
    #[serde(default = "default_srt_bind")]
    pub bind: SocketAddr,
    #[serde(default = "default_srt_latency_ms")]
    pub latency_ms: u32,
    /// AES passphrase callers must use; none means unencrypted.
    pub passphrase: Option<String>,
}

impl Default for SrtSection {
    fn default() -> Self {
        Self { bind: default_srt_bind(), latency_ms: default_srt_latency_ms(), passphrase: None }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct HlsSection {
    #[serde(default = "default_part_ms")]
    pub part_ms: u32,
    #[serde(default = "default_segment_ms")]
    pub segment_ms: u32,
}

impl Default for HlsSection {
    fn default() -> Self {
        Self { part_ms: default_part_ms(), segment_ms: default_segment_ms() }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct BufferSection {
    #[serde(default = "default_window_secs")]
    pub window_secs: u64,
    #[serde(default = "default_max_mb")]
    pub max_mb: usize,
}

impl Default for BufferSection {
    fn default() -> Self {
        Self { window_secs: default_window_secs(), max_mb: default_max_mb() }
    }
}

impl BufferSection {
    pub fn to_buffer_config(self) -> BufferConfig {
        BufferConfig { window: Duration::from_secs(self.window_secs), max_bytes: self.max_mb * 1024 * 1024 }
    }
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub server: ServerSection,
    pub rtmp: RtmpSection,
    pub srt: SrtSection,
    pub hls: HlsSection,
    pub buffer: BufferSection,
    pub tls: TlsSection,
    pub auth: AuthSection,
    pub hooks: HooksSection,
    pub webrtc: WebRtcSection,
}

fn default_webrtc_udp() -> SocketAddr {
    "0.0.0.0:8189".parse().unwrap()
}

/// WHIP ingest and WHEP playback over one UDP port.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct WebRtcSection {
    #[serde(default = "default_webrtc_udp")]
    pub udp_bind: SocketAddr,
    /// Public addresses to advertise when behind NAT.
    pub public_ips: Vec<std::net::IpAddr>,
}

impl Default for WebRtcSection {
    fn default() -> Self {
        Self { udp_bind: default_webrtc_udp(), public_ips: Vec::new() }
    }
}

/// HTTPS next to plain HTTP. Either `cert` + `key` files, or `acme_domains`
/// for automatic Let's Encrypt certificates. Absent `bind`: no HTTPS.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct TlsSection {
    pub bind: Option<SocketAddr>,
    pub cert: Option<std::path::PathBuf>,
    pub key: Option<std::path::PathBuf>,
    pub acme_domains: Vec<String>,
    pub acme_email: Option<String>,
    pub acme_cache_dir: Option<std::path::PathBuf>,
    pub acme_staging: bool,
}

impl TlsSection {
    /// `Ok(None)` when HTTPS is off; an error names what is missing.
    pub fn to_tls_config(&self) -> Result<Option<caudal_tls::TlsConfig>, String> {
        let Some(bind) = self.bind else {
            if self.cert.is_some() || self.key.is_some() || !self.acme_domains.is_empty() {
                return Err("[tls] has certificates but no `bind` address".into());
            }
            return Ok(None);
        };
        let source = match (&self.cert, &self.key, self.acme_domains.is_empty()) {
            (Some(cert), Some(key), true) => caudal_tls::CertSource::Files { cert: cert.clone(), key: key.clone() },
            (None, None, false) => caudal_tls::CertSource::Acme {
                domains: self.acme_domains.clone(),
                email: self.acme_email.clone(),
                cache_dir: self.acme_cache_dir.clone().unwrap_or_else(|| "acme-cache".into()),
                staging: self.acme_staging,
            },
            (Some(_), Some(_), false) => {
                return Err("[tls] set either `cert` + `key` or `acme_domains`, not both".into());
            }
            _ => return Err("[tls] needs `cert` and `key`, or `acme_domains`".into()),
        };
        Ok(Some(caudal_tls::TlsConfig { bind, source }))
    }
}

fn default_jwks_refresh_secs() -> u64 {
    300
}

fn default_true() -> bool {
    true
}

/// Tokens (JWT) to publish and play. Without `secret` or `jwks_url`,
/// everything is open (fine on localhost, not on the internet).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct AuthSection {
    /// Shared HS256 secret.
    pub secret: Option<String>,
    /// JWKS endpoint for RS256 / ES256 / EdDSA keys.
    pub jwks_url: Option<String>,
    #[serde(default = "default_jwks_refresh_secs")]
    pub jwks_refresh_secs: u64,
    /// Require a token to publish (default true once keys are set).
    #[serde(default = "default_true")]
    pub publish: bool,
    /// Require a token to play (default false).
    pub play: bool,
}

impl Default for AuthSection {
    fn default() -> Self {
        Self {
            secret: None,
            jwks_url: None,
            jwks_refresh_secs: default_jwks_refresh_secs(),
            publish: true,
            play: false,
        }
    }
}

impl AuthSection {
    pub fn to_auth_config(&self) -> Result<caudal_auth::AuthConfig, String> {
        let keys = match (&self.secret, &self.jwks_url) {
            (Some(_), Some(_)) => return Err("[auth] set either `secret` or `jwks_url`, not both".into()),
            (Some(s), None) if s.len() < 32 => return Err("[auth] `secret` must be at least 32 characters".into()),
            (Some(s), None) => Some(caudal_auth::KeySource::Secret(s.clone())),
            (None, Some(url)) => Some(caudal_auth::KeySource::Jwks {
                url: url.clone(),
                refresh: Duration::from_secs(self.jwks_refresh_secs.max(10)),
            }),
            (None, None) => None,
        };
        Ok(caudal_auth::AuthConfig { keys, publish: self.publish, play: self.play })
    }
}

/// Signed webhooks (Standard Webhooks) on stream start and end.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct HooksSection {
    pub urls: Vec<String>,
    /// `whsec_...`
    pub secret: Option<String>,
}

impl HooksSection {
    pub fn to_hooks_config(&self) -> Result<Option<caudal_auth::HooksConfig>, String> {
        match (self.urls.is_empty(), &self.secret) {
            (true, _) => Ok(None),
            (false, Some(secret)) => {
                Ok(Some(caudal_auth::HooksConfig { urls: self.urls.clone(), secret: secret.clone() }))
            }
            (false, None) => Err("[hooks] `urls` need a `secret` (whsec_...) to sign with".into()),
        }
    }
}

impl Config {
    /// Checks that cut across keys; run by `load` and `caudal check`.
    pub fn validate(&self) -> Result<(), String> {
        self.tls.to_tls_config()?;
        self.auth.to_auth_config()?;
        self.hooks.to_hooks_config()?;
        Ok(())
    }
}

/// Loads and validates a config file, rejecting unknown keys. The error
/// message names the offending key and its line number (from `toml`'s
/// span-aware `Display` impl).
pub fn load(path: &Path) -> Result<Config, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let cfg: Config = toml::from_str(&text).map_err(|e| e.to_string())?;
    cfg.validate()?;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_empty() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.server.http_bind, default_http_bind());
        assert_eq!(cfg.rtmp.bind, default_rtmp_bind());
        assert_eq!(cfg.rtmp.app, "live");
        assert_eq!(cfg.hls.part_ms, 200);
        assert_eq!(cfg.hls.segment_ms, 2000);
        assert_eq!(cfg.buffer.window_secs, 50);
        assert_eq!(cfg.buffer.max_mb, 256);
    }

    #[test]
    fn partial_section_keeps_other_defaults() {
        let cfg: Config = toml::from_str("[hls]\npart_ms = 100\n").unwrap();
        assert_eq!(cfg.hls.part_ms, 100);
        assert_eq!(cfg.hls.segment_ms, 2000);
    }

    #[test]
    fn unknown_key_reports_name_and_line() {
        let src = "[server]\nhttp_bind = \"127.0.0.1:0\"\nhttp_bnid = 1\n";
        let err = toml::from_str::<Config>(src).unwrap_err().to_string();
        assert!(err.contains("http_bnid"), "{err}");
        assert!(err.contains("line 3"), "{err}");
    }

    #[test]
    fn buffer_section_converts_to_core_config() {
        let cfg: Config = toml::from_str("[buffer]\nwindow_secs = 10\nmax_mb = 1\n").unwrap();
        let bc = cfg.buffer.to_buffer_config();
        assert_eq!(bc.window, Duration::from_secs(10));
        assert_eq!(bc.max_bytes, 1024 * 1024);
    }

    #[test]
    fn tls_auth_hooks_validation() {
        let bad = |t: &str| toml::from_str::<Config>(t).unwrap().validate().unwrap_err();
        assert!(bad("[tls]\nbind = \"0.0.0.0:8443\"").contains("cert"));
        assert!(bad("[tls]\ncert = \"a.pem\"\nkey = \"k.pem\"").contains("bind"));
        assert!(bad("[auth]\nsecret = \"short\"").contains("32"));
        assert!(bad("[hooks]\nurls = [\"https://x\"]").contains("secret"));
        let ok: Config = toml::from_str("[tls]\nbind = \"0.0.0.0:8443\"\ncert = \"c.pem\"\nkey = \"k.pem\"\n[auth]\nsecret = \"0123456789abcdef0123456789abcdef\"\nplay = true").unwrap();
        assert!(ok.validate().is_ok());
        assert!(ok.auth.publish, "publish defaults to required once keys exist");
    }

    #[test]
    fn load_missing_file_is_an_error_not_a_panic() {
        let err = load(Path::new("/nonexistent/caudal.toml")).unwrap_err();
        assert!(err.contains("nonexistent"), "{err}");
    }
}
