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
    /// Reverse proxies allowed to set `X-Forwarded-For` for the resolved
    /// client address, CIDRs or bare addresses. HTTP protocols only (LL-HLS,
    /// WHIP/WHEP, recordings/VOD/clips, the channel skip endpoint): RTMP,
    /// SRT, RTSP and MoQ have no such header and always see the raw
    /// transport peer. Empty (the default): every peer is trusted as the
    /// real client, i.e. `X-Forwarded-For` is never honored.
    pub trusted_proxies: Vec<String>,
}

impl Default for ServerSection {
    fn default() -> Self {
        Self { http_bind: default_http_bind(), trusted_proxies: Vec::new() }
    }
}

impl ServerSection {
    pub fn trusted_proxy_cidrs(&self) -> Result<Vec<caudal_core::Cidr>, String> {
        self.trusted_proxies
            .iter()
            .map(|s| caudal_core::Cidr::parse(s).map_err(|e| format!("[server] trusted_proxies: {e}")))
            .collect()
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
    /// Legacy `EXT-X-CUE-OUT` / `-CONT` / `EXT-X-CUE-IN` for SSAI vendors.
    #[serde(default)]
    pub cue_out_tags: bool,
    /// Seconds a playlist stays live after its publisher drops; a republish
    /// of the same name within them continues it. 0 ends it at once.
    #[serde(default = "default_reconnect_grace_secs")]
    pub reconnect_grace_secs: u32,
}

fn default_reconnect_grace_secs() -> u32 {
    10
}

impl Default for HlsSection {
    fn default() -> Self {
        Self {
            part_ms: default_part_ms(),
            segment_ms: default_segment_ms(),
            cue_tags: true,
            cue_out_tags: false,
            reconnect_grace_secs: default_reconnect_grace_secs(),
        }
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
    pub access: AccessSection,
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
    /// Backup sources: `[[failover]]` entries, each a public stream fed by
    /// the best healthy source of an ordered list.
    pub failover: Vec<FailoverEntry>,
    /// Admin login for the UI and management API. Absent: no login, and
    /// the server refuses to listen on a non-loopback address.
    pub admin: Option<caudal_admin::AdminSection>,
    pub health: HealthSection,
    /// Origin-edge clustering. Absent: a standalone server.
    pub cluster: Option<ClusterSection>,
}

/// `[cluster]`: this node's part in an origin-edge cluster (see
/// `docs/research/CLUSTER.md`). Origins take publishers and serve their
/// streams to edges over MoQ; edges pull a stream from the first origin
/// that has it when their first viewer asks for it.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClusterSection {
    pub role: ClusterRole,
    /// Names this node in inter-node tokens and logs (default: `caudal`).
    #[serde(default)]
    pub node_id: Option<String>,
    /// Shared by every node of the cluster; signs inter-node tokens.
    pub secret: String,
    /// Edge only: HTTP base URLs of the origins (`http://origin-a:8080`),
    /// tried in order.
    #[serde(default)]
    pub origins: Vec<String>,
    /// Edge only: stop pulling a stream this long after its last viewer.
    #[serde(default = "default_cluster_idle_secs")]
    pub idle_timeout_secs: u64,
    /// Edge only: end a pulled stream when no origin has had it this long.
    #[serde(default = "default_cluster_source_secs")]
    pub source_timeout_secs: u64,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ClusterRole {
    Origin,
    Edge,
}

fn default_cluster_idle_secs() -> u64 {
    30
}

fn default_cluster_source_secs() -> u64 {
    10
}

impl ClusterSection {
    pub fn secret(&self) -> Result<caudal_cluster::Secret, String> {
        caudal_cluster::Secret::new(&self.secret).map_err(|e| e.to_string())
    }

    pub fn node_id(&self) -> String {
        self.node_id.clone().unwrap_or_else(|| "caudal".into())
    }

    /// The edge's runtime config; `None` on an origin.
    pub fn edge_config(&self, buffer: BufferConfig) -> Result<Option<caudal_cluster::EdgeConfig>, String> {
        let secret = self.secret()?;
        if self.role == ClusterRole::Origin {
            if !self.origins.is_empty() {
                return Err(r#"[cluster] `origins` is for role = "edge""#.into());
            }
            return Ok(None);
        }
        if self.origins.is_empty() {
            return Err(r#"[cluster] role = "edge" needs at least one entry in `origins`"#.into());
        }
        let mut origins = Vec::new();
        for o in &self.origins {
            let u: url::Url = o.parse().map_err(|e| format!("[cluster] origin {o:?}: {e}"))?;
            if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none() {
                return Err(format!("[cluster] origin {o:?}: expected an http(s)://host[:port] URL"));
            }
            origins.push(u);
        }
        Ok(Some(caudal_cluster::EdgeConfig {
            node_id: self.node_id(),
            secret,
            origins,
            idle_timeout: Duration::from_secs(self.idle_timeout_secs.max(1)),
            source_timeout: Duration::from_secs(self.source_timeout_secs.max(1)),
            buffer,
        }))
    }
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

/// One failover stream: viewers play `stream`, fed by the first healthy
/// entry of `sources` (ingest stream names, or `file:<path>` for a looping
/// slate).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FailoverEntry {
    pub stream: String,
    pub sources: Vec<String>,
    /// No frames from the active source for this long: switch.
    #[serde(default = "default_switch_after_ms")]
    pub switch_after_ms: u64,
    /// A better-ranked source healthy this long: switch back.
    #[serde(default = "default_switch_back_after_secs")]
    pub switch_back_after_secs: u64,
}

fn default_switch_after_ms() -> u64 {
    2000
}

fn default_switch_back_after_secs() -> u64 {
    10
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

    /// `[[rtsp.pull]]` as the runtime type `caudal-rtsp` takes.
    pub fn pull_targets(&self) -> Vec<caudal_rtsp::RtspPull> {
        self.pull.iter().map(|p| caudal_rtsp::RtspPull { stream: p.stream.clone(), url: p.url.clone() }).collect()
    }
}

impl SrtSection {
    /// `[[srt.push]]` as the runtime type `caudal-srt` takes.
    pub fn push_targets(&self) -> Vec<caudal_srt::SrtPush> {
        self.push.iter().map(|p| caudal_srt::SrtPush { stream: p.stream.clone(), url: p.url.clone() }).collect()
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

/// One `[[record.schedule]]` entry: a start/stop window in time for streams
/// not already covered by `[record] streams`. Either `start` (a one-off
/// RFC 3339 instant) or `cron` (a recurring 5-field crontab, `min hour dom
/// month dow`, e.g. `"0 18 * * MON-FRI"`, evaluated in `tz`) — not both.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ScheduleSection {
    /// Stream names or `prefix*` patterns this window applies to.
    pub streams: Vec<String>,
    pub start: Option<String>,
    pub cron: Option<String>,
    /// IANA zone name for `cron` (default UTC). Ignored for `start`, which
    /// already carries its own offset.
    pub tz: Option<String>,
    /// Seconds, not minutes: lets a short window (or a fast end-to-end
    /// test) use less than a minute; an hour-long show just passes 3600.
    pub duration_secs: u32,
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
    /// `[[record.schedule]]`: start/stop windows for streams not in
    /// `streams` above (which always means "always record").
    pub schedule: Vec<ScheduleSection>,
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
            schedule: Vec::new(),
        }
    }
}

impl RecordSection {
    pub fn to_record_config(&self) -> Result<Option<caudal_record::RecordConfig>, String> {
        if !self.enabled {
            return Ok(None);
        }
        let schedules: Vec<caudal_record::ScheduleConfig> = self
            .schedule
            .iter()
            .map(|s| caudal_record::ScheduleConfig {
                streams: s.streams.clone(),
                start: s.start.clone(),
                cron: s.cron.clone(),
                tz: s.tz.clone(),
                duration_secs: s.duration_secs,
            })
            .collect();
        caudal_record::validate_schedules(&schedules)?;
        Ok(Some(caudal_record::RecordConfig {
            dir: self.dir.clone(),
            streams: self.streams.clone(),
            segment_secs: self.segment_secs.max(1),
            retention_hours: self.retention_hours,
            upload_url: self.upload_url.clone(),
            schedules,
        }))
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
    /// `buffer`: for streams ingested over MoQ (`publish/<name>`), the same
    /// `[buffer]` every other ingest protocol uses.
    pub fn to_moq_config(&self, buffer: caudal_core::BufferConfig) -> Result<Option<caudal_moq::MoqConfig>, String> {
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
        Ok(Some(caudal_moq::MoqConfig { bind: self.bind, cert, buffer }))
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
    /// Peer engines sharing the UDP port (cores doing SRTP). 0: one per
    /// core, at most 8.
    pub threads: usize,
}

impl Default for WebRtcSection {
    fn default() -> Self {
        Self { udp_bind: default_webrtc_udp(), public_ips: Vec::new(), threads: 0 }
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

/// Per-stream IP/CIDR and country allow/deny rules (geo-blocking), enforced
/// on every protocol through `Registry::authorize`. See `caudal_access`'s
/// crate docs for the precedence rules (first matching `[[access.rules]]`
/// entry wins; deny beats allow within it).
///
/// `[[access]]` (a bare top-level array) can't coexist with `[access]`
/// `geoip_db` in TOML — the key `access` can't be both a table and an
/// array of tables — so rules nest as `[[access.rules]]`, the same
/// dotted-array convention already used by `[[admin.users]]`,
/// `[[health.stream]]` and `[[transcode.ladder]]` in this file.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct AccessSection {
    /// MaxMind DB or DB-IP `.mmdb` file for `country:` entries. Required
    /// when any rule below has one; the operator's own file, never
    /// bundled or downloaded.
    pub geoip_db: Option<std::path::PathBuf>,
    pub rules: Vec<AccessRuleEntry>,
}

/// One `[[access.rules]]` entry: `streams` glob patterns (trailing `*` is a
/// prefix match, same convention as `[auth]` token `sub` claims) plus the
/// four allow/deny lists, each a CIDR (bare address = host route) or
/// `country:XX` (ISO 3166-1 alpha-2).
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct AccessRuleEntry {
    pub streams: Vec<String>,
    pub play_allow: Vec<String>,
    pub play_deny: Vec<String>,
    pub publish_allow: Vec<String>,
    pub publish_deny: Vec<String>,
}

impl AccessSection {
    pub fn to_access_config(&self) -> Result<caudal_access::AccessConfig, String> {
        let parse_list = |xs: &[String]| -> Result<Vec<caudal_access::Entry>, String> {
            xs.iter().map(|s| caudal_access::Entry::parse(s).map_err(|e| format!("[[access.rules]] {e}"))).collect()
        };
        let mut rules = Vec::with_capacity(self.rules.len());
        for r in &self.rules {
            if r.streams.is_empty() {
                return Err("[[access.rules]] needs at least one `streams` pattern".into());
            }
            rules.push(caudal_access::Rule {
                streams: r.streams.clone(),
                play_allow: parse_list(&r.play_allow)?,
                play_deny: parse_list(&r.play_deny)?,
                publish_allow: parse_list(&r.publish_allow)?,
                publish_deny: parse_list(&r.publish_deny)?,
            });
        }
        let cfg = caudal_access::AccessConfig { rules, geoip_db: self.geoip_db.clone() };
        cfg.validate()?;
        Ok(cfg)
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

/// Stream health alerts: no keyframes, bitrate below a floor, publisher
/// lost, optionally no audio. Signed webhooks on the same wire format as
/// `[hooks]` (Standard Webhooks); a Slack incoming-webhook URL also works
/// (the payload carries a `text` field).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct HealthSection {
    /// No keyframe for this long: alert. `0` disables the rule.
    #[serde(default = "default_no_keyframe_secs")]
    pub no_keyframe_secs: u64,
    /// Bitrate floor in kbps; unset disables the rule.
    pub min_bitrate_kbps: Option<u32>,
    /// How long the bitrate must stay under the floor before it counts.
    #[serde(default = "default_min_bitrate_for_secs")]
    pub min_bitrate_for_secs: u64,
    /// No audio frame for this long; unset disables the rule. Only ever
    /// evaluated for streams that declare an audio track.
    pub no_audio_secs: Option<u64>,
    #[serde(default = "default_true")]
    pub publisher_lost: bool,
    /// Grace window for a republish under the same name before
    /// `publisher_lost` counts a disconnect as real. `caudal-core` has no
    /// signal distinguishing a clean unpublish from a dropped connection
    /// (see `caudal-health`'s crate docs), so this grace window is the
    /// whole decision: back in time, no alert; not back, alert.
    #[serde(default = "default_publisher_lost_grace_secs")]
    pub publisher_lost_grace_secs: u64,
    /// Hysteresis: how long a rule must be continuously good before its
    /// `resolved` fires. Keeps a value bouncing near a threshold from
    /// flapping alert/resolved.
    #[serde(default = "default_health_min_hold_secs")]
    pub min_hold_secs: u64,
    pub webhooks: Vec<String>,
    /// `whsec_...`
    pub secret: Option<String>,
    #[serde(rename = "stream")]
    pub streams: Vec<HealthStreamOverride>,
}

impl Default for HealthSection {
    fn default() -> Self {
        Self {
            no_keyframe_secs: default_no_keyframe_secs(),
            min_bitrate_kbps: None,
            min_bitrate_for_secs: default_min_bitrate_for_secs(),
            no_audio_secs: None,
            publisher_lost: true,
            publisher_lost_grace_secs: default_publisher_lost_grace_secs(),
            min_hold_secs: default_health_min_hold_secs(),
            webhooks: Vec::new(),
            secret: None,
            streams: Vec::new(),
        }
    }
}

fn default_no_keyframe_secs() -> u64 {
    10
}

fn default_min_bitrate_for_secs() -> u64 {
    10
}

fn default_publisher_lost_grace_secs() -> u64 {
    5
}

fn default_health_min_hold_secs() -> u64 {
    5
}

/// `[[health.stream]]`: overrides the matching default for one stream name.
/// A field left unset inherits the `[health]` default.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HealthStreamOverride {
    pub name: String,
    #[serde(default)]
    pub no_keyframe_secs: Option<u64>,
    #[serde(default)]
    pub min_bitrate_kbps: Option<u32>,
    #[serde(default)]
    pub min_bitrate_for_secs: Option<u64>,
    #[serde(default)]
    pub no_audio_secs: Option<u64>,
    #[serde(default)]
    pub publisher_lost: Option<bool>,
    #[serde(default)]
    pub publisher_lost_grace_secs: Option<u64>,
    #[serde(default)]
    pub min_hold_secs: Option<u64>,
}

impl HealthSection {
    pub fn to_health_config(&self) -> Result<Option<caudal_health::HealthConfig>, String> {
        if self.webhooks.is_empty() {
            return Ok(None);
        }
        let secret = self
            .secret
            .clone()
            .ok_or_else(|| "[health] `webhooks` need a `secret` (whsec_...) to sign with".to_string())?;
        let mut seen = std::collections::HashSet::new();
        let mut overrides = Vec::with_capacity(self.streams.len());
        for o in &self.streams {
            if !caudal_core::media::valid_stream_name(&o.name) {
                return Err(format!("[[health.stream]] name `{}` is not a valid stream name", o.name));
            }
            if !seen.insert(o.name.clone()) {
                return Err(format!("[[health.stream]] duplicate name `{}`", o.name));
            }
            overrides.push(caudal_health::StreamOverride {
                name: o.name.clone(),
                no_keyframe_secs: o.no_keyframe_secs,
                min_bitrate_kbps: o.min_bitrate_kbps,
                min_bitrate_for_secs: o.min_bitrate_for_secs,
                no_audio_secs: o.no_audio_secs,
                publisher_lost: o.publisher_lost,
                publisher_lost_grace_secs: o.publisher_lost_grace_secs,
                min_hold_secs: o.min_hold_secs,
            });
        }
        Ok(Some(caudal_health::HealthConfig {
            no_keyframe_secs: (self.no_keyframe_secs > 0).then_some(self.no_keyframe_secs),
            min_bitrate_kbps: self.min_bitrate_kbps,
            min_bitrate_for_secs: self.min_bitrate_for_secs.max(1),
            no_audio_secs: self.no_audio_secs,
            publisher_lost: self.publisher_lost,
            publisher_lost_grace_secs: self.publisher_lost_grace_secs,
            min_hold_secs: self.min_hold_secs.max(1),
            webhooks: self.webhooks.clone(),
            secret,
            overrides,
        }))
    }
}

impl Config {
    /// Checks that cut across keys; run by `load` and `caudal check`.
    pub fn validate(&self) -> Result<(), String> {
        self.tls.to_tls_config()?;
        self.auth.to_auth_config()?;
        self.access.to_access_config()?;
        self.server.trusted_proxy_cidrs()?;
        self.hooks.to_hooks_config()?;
        self.moq.to_moq_config(self.buffer.to_buffer_config())?;
        self.record.to_record_config()?;
        self.transcode.to_transcode_config(self.buffer.to_buffer_config())?;
        self.rtsp.to_tls_config()?;
        if let Some(admin) = &self.admin {
            admin.validate()?;
        }
        self.health.to_health_config()?;
        if let Some(c) = &self.cluster {
            c.edge_config(self.buffer.to_buffer_config())?;
            if c.role == ClusterRole::Origin && !self.moq.enabled {
                return Err(r#"[cluster] role = "origin" needs [moq] enabled (edges pull over MoQ)"#.into());
            }
        }
        self.failovers()?;
        Ok(())
    }

    /// `[[failover]]` as the runtime type `caudal-failover` takes, checked:
    /// valid names and sources, one entry per stream, and no stream that a
    /// `[[channel]]` also publishes.
    pub fn failovers(&self) -> Result<Vec<caudal_failover::Failover>, String> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::with_capacity(self.failover.len());
        for e in &self.failover {
            let sources = e
                .sources
                .iter()
                .map(|s| caudal_failover::Source::parse(s).map_err(|err| format!("[[failover]] `{}`: {err}", e.stream)))
                .collect::<Result<Vec<_>, _>>()?;
            let f = caudal_failover::Failover {
                stream: e.stream.clone(),
                sources,
                switch_after: Duration::from_millis(e.switch_after_ms),
                switch_back_after: Duration::from_secs(e.switch_back_after_secs),
            };
            f.validate()?;
            if !seen.insert(e.stream.clone()) {
                return Err(format!("[[failover]] duplicate stream `{}`", e.stream));
            }
            if self.channel.iter().any(|c| c.name == e.stream) {
                return Err(format!("[[failover]] stream `{}` is also a [[channel]]", e.stream));
            }
            out.push(f);
        }
        Ok(out)
    }

    /// `[[restream]]` as the runtime type `caudal-restream` takes.
    pub fn restream_targets(&self) -> Vec<caudal_restream::RestreamTarget> {
        self.restream
            .iter()
            .map(|r| caudal_restream::RestreamTarget { stream: r.stream.clone(), url: r.url.clone() })
            .collect()
    }

    /// `[[channel]]` as the runtime type `caudal-channel` takes.
    pub fn channels(&self) -> Vec<caudal_channel::Channel> {
        self.channel
            .iter()
            .map(|c| caudal_channel::Channel {
                name: c.name.clone(),
                items: c.items.clone(),
                r#loop: c.r#loop,
                shuffle: c.shuffle,
            })
            .collect()
    }

    /// The transcode config `caudal-transcode` takes, even with no ladders
    /// configured (an idle subscriber, so a later reload can add ladders
    /// without a restart).
    pub fn transcode_runtime_config(&self, buffer: BufferConfig) -> caudal_transcode::TranscodeConfig {
        self.transcode.to_transcode_config(buffer).expect("validated").unwrap_or_else(|| {
            caudal_transcode::TranscodeConfig {
                ladders: Vec::new(),
                engine: caudal_transcode::Engine::Ffmpeg,
                ffmpeg: self.transcode.ffmpeg.clone(),
                buffer,
            }
        })
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
    fn failover_entries_parse_and_validate() {
        let cfg: Config = toml::from_str(
            "[[failover]]\nstream = \"main\"\nsources = [\"main-primary\", \"main-backup\", \"file:/slate.mp4\"]\n",
        )
        .unwrap();
        let f = &cfg.failovers().unwrap()[0];
        assert_eq!(f.switch_after, Duration::from_millis(2000));
        assert_eq!(f.switch_back_after, Duration::from_secs(10));
        assert_eq!(f.sources[2], caudal_failover::Source::File("/slate.mp4".into()));

        let bad = |t: &str| toml::from_str::<Config>(t).unwrap().validate().unwrap_err();
        assert!(bad("[[failover]]\nstream = \"main\"\nsources = []\n").contains("at least one source"));
        assert!(bad("[[failover]]\nstream = \"main\"\nsources = [\"a b\"]\n").contains("neither"));
        let twice =
            "[[failover]]\nstream = \"main\"\nsources = [\"a\"]\n[[failover]]\nstream = \"main\"\nsources = [\"b\"]\n";
        assert!(bad(twice).contains("duplicate"));
        let channel = "[[channel]]\nname = \"main\"\nitems = []\n[[failover]]\nstream = \"main\"\nsources = [\"a\"]\n";
        assert!(bad(channel).contains("[[channel]]"));
    }

    #[test]
    fn partial_section_keeps_other_defaults() {
        let cfg: Config = toml::from_str("[hls]\npart_ms = 100\n").unwrap();
        assert_eq!(cfg.hls.part_ms, 100);
        assert_eq!(cfg.hls.segment_ms, 2000);
        assert!(cfg.hls.cue_tags, "cue tags default on");
        let cfg: Config = toml::from_str("[hls]\ncue_tags = false\n").unwrap();
        assert!(!cfg.hls.cue_tags);
        assert!(!cfg.hls.cue_out_tags, "legacy cue tags default off");
        let cfg: Config = toml::from_str("[hls]\ncue_out_tags = true\n").unwrap();
        assert!(cfg.hls.cue_out_tags);
        assert_eq!(cfg.hls.reconnect_grace_secs, 10, "reconnect grace defaults to 10 s");
        let cfg: Config = toml::from_str("[hls]\nreconnect_grace_secs = 0\n").unwrap();
        assert_eq!(cfg.hls.reconnect_grace_secs, 0);
    }

    #[test]
    fn cluster_section() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.cluster.is_none(), "standalone by default");

        let edge = r#"[cluster]
role = "edge"
secret = "0123456789abcdef"
origins = ["http://origin-a:8080", "https://origin-b"]
"#;
        let cfg: Config = toml::from_str(edge).unwrap();
        cfg.validate().unwrap();
        let c = cfg.cluster.as_ref().unwrap();
        assert_eq!(c.node_id(), "caudal");
        let e = c.edge_config(BufferConfig::default()).unwrap().expect("edge");
        assert_eq!(e.origins.len(), 2);
        assert_eq!(e.idle_timeout, Duration::from_secs(30));
        assert_eq!(e.source_timeout, Duration::from_secs(10));

        let origin = "[cluster]\nrole = \"origin\"\nnode_id = \"o1\"\nsecret = \"0123456789abcdef\"\n";
        let cfg: Config = toml::from_str(origin).unwrap();
        cfg.validate().unwrap();
        assert!(cfg.cluster.as_ref().unwrap().edge_config(BufferConfig::default()).unwrap().is_none());

        let bad = |t: &str| toml::from_str::<Config>(t).unwrap().validate().unwrap_err();
        assert!(bad("[cluster]\nrole = \"edge\"\nsecret = \"0123456789abcdef\"\n").contains("origins"));
        assert!(bad("[cluster]\nrole = \"edge\"\nsecret = \"short\"\norigins = [\"http://a\"]\n").contains("16"));
        assert!(
            bad("[cluster]\nrole = \"edge\"\nsecret = \"0123456789abcdef\"\norigins = [\"rtmp://a\"]\n")
                .contains("http")
        );
        assert!(bad(&format!("{origin}origins = [\"http://a\"]\n")).contains("edge"));
        assert!(bad(&format!("[moq]\nenabled = false\n\n{origin}")).contains("[moq]"));
        assert!(toml::from_str::<Config>("[cluster]\nrole = \"relay\"\nsecret = \"x\"\n").is_err());
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
    fn access_section_parses_nested_rules() {
        // No `country:` entry here, so no `geoip_db` is needed to validate
        // (that combination is `access_section_with_geoip_db_validates`,
        // below, which needs a real openable fixture file).
        let src = "[[access.rules]]\nstreams = [\"live-*\"]\nplay_deny = [\"203.0.113.0/24\"]\npublish_allow = [\"10.0.0.0/8\"]\n";
        let cfg: Config = toml::from_str(src).unwrap();
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.access.rules.len(), 1);
        assert_eq!(cfg.access.rules[0].streams, vec!["live-*".to_string()]);
        let runtime = cfg.access.to_access_config().unwrap();
        assert_eq!(runtime.rules.len(), 1);
    }

    #[test]
    fn access_section_with_geoip_db_validates() {
        use std::io::Write as _;
        // A minimal but real MaxMind DB, built the same way
        // `caudal-access`'s own tests do (see crates/caudal-access/src/geo.rs).
        let mut db = maxminddb_writer::Database::default();
        db.metadata.binary_format_major_version = 2;
        db.metadata.database_type = "GeoIP2-Country-Test".to_owned();
        let data = db.insert_value(std::collections::BTreeMap::from([("x", 1u32)])).unwrap();
        db.insert_node("0.0.0.0/0".parse::<maxminddb_writer::paths::IpAddrWithMask>().unwrap(), data);
        let bytes = db.write_to(Vec::new()).unwrap();
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&bytes).unwrap();

        let src = format!(
            "[access]\ngeoip_db = {:?}\n[[access.rules]]\nstreams = [\"*\"]\nplay_deny = [\"country:KP\"]\n",
            file.path()
        );
        let cfg: Config = toml::from_str(&src).unwrap();
        assert!(cfg.validate().is_ok(), "{:?}", cfg.validate());
    }

    #[test]
    fn access_country_rule_without_geoip_db_is_a_config_error() {
        let src = "[[access.rules]]\nstreams = [\"*\"]\nplay_deny = [\"country:KP\"]\n";
        let cfg: Config = toml::from_str(src).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("geoip_db"), "{err}");
    }

    #[test]
    fn access_rule_needs_a_streams_pattern() {
        let src = "[[access.rules]]\nplay_deny = [\"1.2.3.4\"]\n";
        let cfg: Config = toml::from_str(src).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("streams"), "{err}");
    }

    #[test]
    fn access_rule_rejects_a_bad_entry() {
        let src = "[[access.rules]]\nstreams = [\"*\"]\nplay_deny = [\"not-a-cidr-or-country\"]\n";
        let cfg: Config = toml::from_str(src).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn access_unknown_key_reports_name_and_line() {
        let src = "[[access.rules]]\nstreams = [\"*\"]\nplay_dney = [\"1.2.3.4\"]\n";
        let err = toml::from_str::<Config>(src).unwrap_err().to_string();
        assert!(err.contains("play_dney"), "{err}");
        assert!(err.contains("line 3"), "{err}");
    }

    #[test]
    fn trusted_proxies_parses_and_rejects_garbage() {
        let cfg: Config = toml::from_str("[server]\ntrusted_proxies = [\"10.0.0.0/8\", \"192.168.1.1\"]\n").unwrap();
        assert_eq!(cfg.server.trusted_proxy_cidrs().unwrap().len(), 2);

        let bad: Config = toml::from_str("[server]\ntrusted_proxies = [\"not-an-ip\"]\n").unwrap();
        let err = bad.validate().unwrap_err();
        assert!(err.contains("trusted_proxies"), "{err}");
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

    #[test]
    fn health_section_defaults_and_disabled_without_webhooks() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.health.no_keyframe_secs, 10);
        assert!(cfg.health.publisher_lost);
        assert!(cfg.health.to_health_config().unwrap().is_none(), "no webhooks configured => off");
    }

    #[test]
    fn health_section_needs_a_secret_and_valid_stream_names() {
        let err = toml::from_str::<Config>("[health]\nwebhooks = [\"https://x\"]").unwrap().validate().unwrap_err();
        assert!(err.contains("secret"), "{err}");

        let cfg: Config = toml::from_str(
            "[health]\nwebhooks = [\"https://x\"]\nsecret = \"whsec_abc\"\n[[health.stream]]\nname = \"a-b\"\nno_keyframe_secs = 3\n",
        )
        .unwrap();
        let hc = cfg.health.to_health_config().unwrap().unwrap();
        assert_eq!(hc.overrides.len(), 1);
        assert_eq!(hc.overrides[0].no_keyframe_secs, Some(3));

        let bad = toml::from_str::<Config>(
            "[health]\nwebhooks = [\"https://x\"]\nsecret = \"whsec_abc\"\n[[health.stream]]\nname = \"not a valid name\"\n",
        )
        .unwrap();
        assert!(bad.health.to_health_config().unwrap_err().contains("valid stream name"));
    }

    #[test]
    fn health_no_keyframe_secs_zero_disables_the_rule() {
        let cfg: Config =
            toml::from_str("[health]\nno_keyframe_secs = 0\nwebhooks = [\"https://x\"]\nsecret = \"whsec_abc\"\n")
                .unwrap();
        let hc = cfg.health.to_health_config().unwrap().unwrap();
        assert_eq!(hc.no_keyframe_secs, None);
    }
}
