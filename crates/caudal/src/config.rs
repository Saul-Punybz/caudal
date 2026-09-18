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
    /// Push streams out to remote SRT listeners.
    pub push: Vec<SrtPushEntry>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SrtPushEntry {
    pub stream: String,
    pub url: String,
}

impl Default for SrtSection {
    fn default() -> Self {
        Self { bind: default_srt_bind(), latency_ms: default_srt_latency_ms(), passphrase: None, push: Vec::new() }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct HlsSection {
    #[serde(default = "default_part_ms")]
    pub part_ms: u32,
    #[serde(default = "default_segment_ms")]
    pub segment_ms: u32,
    /// SCTE-35 cues as `EXT-X-DATERANGE` in the media playlists.
    #[serde(default = "default_true")]
    pub cue_tags: bool,
}

impl Default for HlsSection {
    fn default() -> Self {
        Self { part_ms: default_part_ms(), segment_ms: default_segment_ms(), cue_tags: true }
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
    pub moq: MoqSection,
    pub record: RecordSection,
    pub rtsp: RtspSection,
    pub transcode: TranscodeSection,
    /// 24/7 channels from files: `[[channel]]` entries.
    pub channel: Vec<ChannelEntry>,
    /// Multistreaming: `[[restream]]` entries, each pushing one stream to an
    /// RTMP/RTMPS ingest (YouTube, Twitch, Facebook, another server).
    pub restream: Vec<RestreamEntry>,
    /// Admin login for the UI and management API. Absent: no login, and
    /// the server refuses to listen on a non-loopback address.
    pub admin: Option<caudal_admin::AdminSection>,
}

/// One restream target: push `stream` to `url` (`rtmp://` or `rtmps://`,
/// `host[:port]/app/key`) while it is live. The key never reaches logs or
/// the API.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RestreamEntry {
    pub stream: String,
    pub url: String,
}

/// One 24/7 channel: a playlist of files (or directories) published as a
/// continuous live stream named `name`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChannelEntry {
    pub name: String,
    pub items: Vec<std::path::PathBuf>,
    #[serde(default = "default_true")]
    pub r#loop: bool,
    #[serde(default)]
    pub shuffle: bool,
}

fn default_rtsp_udp_port_range() -> (u16, u16) {
    (8000, 8999)
}

/// RTSP: pull cameras in (`[[rtsp.pull]]`), serve streams out (`bind`),
/// optionally RTSPS alongside it (`tls_bind` + `tls_cert`/`tls_key`). UDP
/// unicast SETUPs allocate an RTP/RTCP port pair from `udp_port_range`;
/// RTSPS never offers UDP (media stays inside the TLS tunnel, interleaved
/// over the same TCP connection as the control channel).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct RtspSection {
    /// e.g. "0.0.0.0:8554"; absent = no RTSP server.
    pub bind: Option<SocketAddr>,
    pub pull: Vec<RtspPullEntry>,
    /// e.g. `[8000, 8999]`.
    #[serde(default = "default_rtsp_udp_port_range")]
    pub udp_port_range: (u16, u16),
    /// e.g. "0.0.0.0:322" (rtsps' IANA port); absent = no RTSPS.
    pub tls_bind: Option<SocketAddr>,
    pub tls_cert: Option<std::path::PathBuf>,
    pub tls_key: Option<std::path::PathBuf>,
}

impl Default for RtspSection {
    fn default() -> Self {
        Self {
            bind: None,
            pull: Vec::new(),
            udp_port_range: default_rtsp_udp_port_range(),
            tls_bind: None,
            tls_cert: None,
            tls_key: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RtspPullEntry {
    pub stream: String,
    pub url: String,
}

impl RtspSection {
    /// `Ok(None)` when RTSPS is off; an error names what is missing.
    pub fn to_tls_config(&self) -> Result<Option<caudal_rtsp::RtspTlsConfig>, String> {
        let Some(bind) = self.tls_bind else {
            if self.tls_cert.is_some() || self.tls_key.is_some() {
                return Err("[rtsp] has tls_cert/tls_key but no tls_bind address".into());
            }
            return Ok(None);
        };
        let (Some(cert), Some(key)) = (self.tls_cert.clone(), self.tls_key.clone()) else {
            return Err("[rtsp] tls_bind needs both tls_cert and tls_key".into());
        };
        Ok(Some(caudal_rtsp::RtspTlsConfig { bind, cert, key }))
    }
}

/// Transcoding ladders: `[[transcode.ladder]]` with `streams` and
/// `[[transcode.ladder.rendition]]` entries.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct TranscodeSection {
    /// "ffmpeg" (default) or "rusty_h264".
    pub engine: String,
    pub ffmpeg: std::path::PathBuf,
    pub ladder: Vec<LadderEntry>,
}

impl Default for TranscodeSection {
    fn default() -> Self {
        Self { engine: "ffmpeg".into(), ffmpeg: "ffmpeg".into(), ladder: Vec::new() }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LadderEntry {
    pub streams: Vec<String>,
    pub rendition: Vec<RenditionEntry>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RenditionEntry {
    pub label: String,
    pub height: u32,
    pub video_kbps: u32,
    #[serde(default = "default_audio_kbps")]
    pub audio_kbps: u32,
}

fn default_audio_kbps() -> u32 {
    128
}

impl TranscodeSection {
    pub fn to_transcode_config(
        &self,
        buffer: BufferConfig,
    ) -> Result<Option<caudal_transcode::TranscodeConfig>, String> {
        if self.ladder.is_empty() {
            return Ok(None);
        }
        let engine = match self.engine.as_str() {
            "ffmpeg" => caudal_transcode::Engine::Ffmpeg,
            "rusty_h264" => caudal_transcode::Engine::RustyH264,
            other => return Err(format!("[transcode] unknown engine `{other}` (ffmpeg or rusty_h264)")),
        };
        for l in &self.ladder {
            for r in &l.rendition {
                if !caudal_core::media::valid_stream_name(&format!("x+{}", r.label)) {
                    return Err(format!(
                        "[transcode] rendition label `{}` is not a valid stream-name fragment",
                        r.label
                    ));
                }
            }
        }
        Ok(Some(caudal_transcode::TranscodeConfig {
            ladders: self
                .ladder
                .iter()
                .map(|l| caudal_transcode::Ladder {
                    streams: l.streams.clone(),
                    renditions: l
                        .rendition
                        .iter()
                        .map(|r| caudal_transcode::Rendition {
                            label: r.label.clone(),
                            height: r.height,
                            video_kbps: r.video_kbps,
                            audio_kbps: r.audio_kbps,
                        })
                        .collect(),
                })
                .collect(),
            engine,
            ffmpeg: self.ffmpeg.clone(),
            buffer,
        }))
    }
}

fn default_record_dir() -> std::path::PathBuf {
    "recordings".into()
}

fn default_record_streams() -> Vec<String> {
    vec!["*".into()]
}

fn default_segment_secs() -> u32 {
    4
}

/// Recording to disk (and optionally object storage), VOD and clips.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct RecordSection {
    pub enabled: bool,
    #[serde(default = "default_record_dir")]
    pub dir: std::path::PathBuf,
    #[serde(default = "default_record_streams")]
    pub streams: Vec<String>,
    #[serde(default = "default_segment_secs")]
    pub segment_secs: u32,
    pub retention_hours: Option<u32>,
    pub upload_url: Option<String>,
}

impl Default for RecordSection {
    fn default() -> Self {
        Self {
            enabled: false,
            dir: default_record_dir(),
            streams: default_record_streams(),
            segment_secs: default_segment_secs(),
            retention_hours: None,
            upload_url: None,
        }
    }
}

impl RecordSection {
    pub fn to_record_config(&self) -> Option<caudal_record::RecordConfig> {
        self.enabled.then(|| caudal_record::RecordConfig {
            dir: self.dir.clone(),
            streams: self.streams.clone(),
            segment_secs: self.segment_secs.max(1),
            retention_hours: self.retention_hours,
            upload_url: self.upload_url.clone(),
        })
    }
}

fn default_moq_bind() -> SocketAddr {
    "0.0.0.0:4443".parse().unwrap()
}

/// Media over QUIC output over WebTransport. Without `cert`/`key` it uses a
/// short-lived self-signed certificate that browsers accept by fingerprint.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct MoqSection {
    pub enabled: bool,
    #[serde(default = "default_moq_bind")]
    pub bind: SocketAddr,
    pub cert: Option<std::path::PathBuf>,
    pub key: Option<std::path::PathBuf>,
    /// Hostnames for the self-signed certificate (default: localhost).
    pub hosts: Vec<String>,
}

impl Default for MoqSection {
    fn default() -> Self {
        Self { enabled: true, bind: default_moq_bind(), cert: None, key: None, hosts: Vec::new() }
    }
}

impl MoqSection {
    pub fn to_moq_config(&self) -> Result<Option<caudal_moq::MoqConfig>, String> {
        if !self.enabled {
            return Ok(None);
        }
        let cert = match (&self.cert, &self.key) {
            (Some(c), Some(k)) => caudal_moq::MoqCert::Files { cert: c.clone(), key: k.clone() },
            (None, None) => caudal_moq::MoqCert::SelfSigned {
                hosts: if self.hosts.is_empty() {
                    vec!["localhost".into(), "127.0.0.1".into()]
                } else {
                    self.hosts.clone()
                },
            },
            _ => return Err("[moq] needs both `cert` and `key`, or neither".into()),
        };
        Ok(Some(caudal_moq::MoqConfig { bind: self.bind, cert }))
    }
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
        self.moq.to_moq_config()?;
        self.transcode.to_transcode_config(self.buffer.to_buffer_config())?;
        self.rtsp.to_tls_config()?;
        if let Some(admin) = &self.admin {
            admin.validate()?;
        }
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
        assert!(cfg.hls.cue_tags, "cue tags default on");
        let cfg: Config = toml::from_str("[hls]\ncue_tags = false\n").unwrap();
        assert!(!cfg.hls.cue_tags);
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
    fn admin_section_parses_and_validates() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.admin.is_none());
        let src = "[admin]\nsession_ttl_secs = 3600\n[[admin.users]]\nname = \"ana\"\npassword_hash = \"$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHRzYWx0$qjSdhc1XA4ycYYxRaDwn2Q0N+Yjwxm0KhdRLkh8i3XE\"\n[[admin.api_tokens]]\nname = \"ci\"\ntoken_sha256 = \"0000000000000000000000000000000000000000000000000000000000000000\"\n[admin.oidc]\nissuer = \"https://idp.example\"\nclient_id = \"caudal\"\nredirect_url = \"https://caudal.example/api/v1/auth/oidc/callback\"\nallowed_groups = [\"ops\"]\n";
        let cfg: Config = toml::from_str(src).unwrap();
        cfg.validate().unwrap();
        let admin = cfg.admin.unwrap();
        assert_eq!(admin.users[0].name, "ana");
        assert_eq!(admin.session_ttl_secs, 3600);
        let err = toml::from_str::<Config>("[admin]\nuser = []\n").unwrap_err().to_string();
        assert!(err.contains("user"), "unknown keys rejected: {err}");
        let bad: Config = toml::from_str("[[admin.users]]\nname = \"a\"\npassword_hash = \"hunter2\"\n").unwrap();
        assert!(bad.validate().unwrap_err().contains("password_hash"));
    }

    #[test]
    fn load_missing_file_is_an_error_not_a_panic() {
        let err = load(Path::new("/nonexistent/caudal.toml")).unwrap_err();
        assert!(err.contains("nonexistent"), "{err}");
    }
}
