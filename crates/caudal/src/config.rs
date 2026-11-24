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

/// 15 s, not MistServer's 50: see `[buffer]` in caudal.example.toml.
fn default_window_secs() -> u64 {
    15
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
    /// Let viewers `play` streams over RTMP (`rtmp://host/app/stream`), as
    /// VLC, ffplay or another server's RTMP pull do. Same auth as every
    /// other output. On by default.
    #[serde(default = "default_true")]
    pub play: bool,
    /// RTMPS publish (`rtmps://host:port/{app}/{stream}`) on this address,
    /// with `[tls]`'s certificate (files or ACME). Off when absent.
    pub tls_bind: Option<SocketAddr>,
    /// Pull streams in from remote RTMP/RTMPS servers (Caudal plays them).
    pub pull: Vec<RtmpPullEntry>,
}

impl Default for RtmpSection {
    fn default() -> Self {
        Self { bind: default_rtmp_bind(), app: default_rtmp_app(), play: true, tls_bind: None, pull: Vec::new() }
    }
}

impl RtmpSection {
    /// RTMPS needs a certificate, and the only place one is configured is
    /// `[tls]`. `Ok(None)` when RTMPS is off.
    pub fn tls_source(&self, tls: &TlsSection) -> Result<Option<(SocketAddr, caudal_tls::CertSource)>, String> {
        let Some(bind) = self.tls_bind else { return Ok(None) };
        if bind == self.bind {
            return Err("[rtmp] tls_bind must differ from bind".into());
        }
        match tls.to_tls_config()? {
            Some(t) if t.bind == bind => {
                Err("[rtmp] tls_bind must differ from [tls] bind (one port, one protocol)".into())
            }
            Some(t) => Ok(Some((bind, t.source))),
            None => Err("[rtmp] tls_bind needs a certificate in [tls] (cert + key, or acme_domains)".into()),
        }
    }
}

impl RtmpSection {
    /// Whether the listeners themselves (`bind`, `app`, `play`, the RTMPS
    /// `tls_bind`) differ: pulls reload on their own without restarting them.
    pub fn listener_changed(&self, other: &Self) -> bool {
        self.bind != other.bind || self.app != other.app || self.play != other.play || self.tls_bind != other.tls_bind
    }

    /// `[[rtmp.pull]]` as the runtime type `caudal-rtmp` takes.
    pub fn pull_targets(&self) -> Vec<caudal_rtmp::RtmpPull> {
        self.pull
            .iter()
            .filter(|p| !p.on_demand)
            .map(|p| caudal_rtmp::RtmpPull { stream: p.stream.clone(), url: p.url.clone() })
            .collect()
    }
}

/// One `[[rtmp.pull]]`: play `url` (`rtmp://host[:port]/app/key`, or
/// `rtmps://`) from a remote RTMP server and publish it as `stream`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RtmpPullEntry {
    pub stream: String,
    pub url: String,
    /// Start this pull only while someone watches: the first viewer's
    /// request starts it, and it stops `idle_secs` after the last viewer
    /// left. See `docs/ON-DEMAND.md`.
    #[serde(default)]
    pub on_demand: bool,
    /// With `on_demand`: seconds without viewers before the pull stops
    /// (default 30, 1..=3600).
    #[serde(default)]
    pub idle_secs: Option<u64>,
}

#[derive(Clone, Deserialize, PartialEq, Eq)]
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
    /// Pull streams in from remote SRT listeners (Caudal calls them).
    pub pull: Vec<SrtPullEntry>,
}

/// Hand-written so `{:?}` never prints `passphrase` in full.
impl std::fmt::Debug for SrtSection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SrtSection")
            .field("bind", &self.bind)
            .field("latency_ms", &self.latency_ms)
            .field("passphrase", &self.passphrase.as_ref().map(|_| "****"))
            .field("push", &self.push)
            .field("pull", &self.pull)
            .finish()
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SrtPushEntry {
    pub stream: String,
    pub url: String,
}

/// One `[[srt.pull]]`: call the SRT listener at `url` and publish what it
/// sends as `stream`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SrtPullEntry {
    pub stream: String,
    pub url: String,
    /// Start this pull only while someone watches: the first viewer's
    /// request starts it, and it stops `idle_secs` after the last viewer
    /// left. See `docs/ON-DEMAND.md`.
    #[serde(default)]
    pub on_demand: bool,
    /// With `on_demand`: seconds without viewers before the pull stops
    /// (default 30, 1..=3600).
    #[serde(default)]
    pub idle_secs: Option<u64>,
}

impl Default for SrtSection {
    fn default() -> Self {
        Self {
            bind: default_srt_bind(),
            latency_ms: default_srt_latency_ms(),
            passphrase: None,
            push: Vec::new(),
            pull: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
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
    /// Also serve classic HLS with MPEG-TS segments at
    /// `/hls/{name}/ts.m3u8` (older set-top boxes, MPEG-2 + MP2 feeds).
    #[serde(default = "default_true")]
    pub ts: bool,
    /// HLS or LL-HLS pulled in (`[[hls.pull]]`): a remote playlist with
    /// MPEG-TS or fMP4/CMAF segments published as a stream. Reloadable, unlike the
    /// output settings above.
    #[serde(default)]
    pub pull: Vec<HlsPullEntry>,
}

/// One `[[hls.pull]]`: poll the playlist at `url`
/// (`https://cdn.example/live/index.m3u8`, a master or a media playlist)
/// and publish its segments as `stream`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HlsPullEntry {
    pub stream: String,
    pub url: String,
    /// Start this pull only while someone watches: the first viewer's
    /// request starts it, and it stops `idle_secs` after the last viewer
    /// left. See `docs/ON-DEMAND.md`.
    #[serde(default)]
    pub on_demand: bool,
    /// With `on_demand`: seconds without viewers before the pull stops
    /// (default 30, 1..=3600).
    #[serde(default)]
    pub idle_secs: Option<u64>,
}

impl HlsSection {
    pub fn pull_targets(&self) -> Vec<caudal_hls_pull::HlsPull> {
        self.pull
            .iter()
            .filter(|p| !p.on_demand)
            .map(|p| caudal_hls_pull::HlsPull { stream: p.stream.clone(), url: p.url.clone() })
            .collect()
    }

    /// The output settings alone: what a change of needs a restart.
    pub fn output(&self) -> Self {
        Self { pull: Vec::new(), ..self.clone() }
    }
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
            ts: true,
            pull: Vec::new(),
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
    /// MPEG-TS over UDP in (`[[udp.pull]]`): unicast or multicast, plain or RTP.
    pub udp: UdpSection,
    /// MPEG-TS served over HTTP(S) in (`[[http.pull]]`): VLC's
    /// `access=http,mux=ts`, an encoder's HTTP output, MistServer's `http-ts://`.
    pub http: HttpSection,
    /// IP multicast output: `[[multicast]]` entries, each sending one
    /// stream to a multicast group as MPEG-TS over UDP or RTP.
    pub multicast: Vec<MulticastEntry>,
    /// Backup sources: `[[failover]]` entries, each a public stream fed by
    /// the best healthy source of an ordered list.
    pub failover: Vec<FailoverEntry>,
    /// Admin login for the UI and management API. Absent: no login, and
    /// the server refuses to listen on a non-loopback address.
    pub admin: Option<caudal_admin::AdminSection>,
    pub health: HealthSection,
    /// Automatic live captions: `[captions]` + `[[captions.stream]]`.
    pub captions: crate::captions::CaptionsSection,
    /// Origin-edge clustering. Absent: a standalone server.
    pub cluster: Option<ClusterSection>,
    /// Open Media Transport: `[omt]`, `[[omt.pull]]`, `[[omt.output]]`.
    pub omt: OmtSection,
    /// Automatic pushes: `[[push.auto]]` rules, each a stream pattern and a
    /// target URL template, started for every matching stream while it is
    /// live (see `crate::auto_push`).
    pub push: PushSection,
}

/// `[cluster]`: this node's part in an origin-edge cluster (see
/// `docs/research/CLUSTER.md`). Origins take publishers and serve their
/// streams to edges over MoQ; edges pull a stream from the first origin
/// that has it when their first viewer asks for it.
#[derive(Clone, Deserialize, PartialEq, Eq)]
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

/// Hand-written so `{:?}` never prints `secret` in full.
impl std::fmt::Debug for ClusterSection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClusterSection")
            .field("role", &self.role)
            .field("node_id", &self.node_id)
            .field("secret", &"****")
            .field("origins", &self.origins)
            .field("idle_timeout_secs", &self.idle_timeout_secs)
            .field("source_timeout_secs", &self.source_timeout_secs)
            .finish()
    }
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
/// `[udp]`: `[[udp.pull]]` (MPEG-TS in) and `[[udp.push]]` (MPEG-TS out to
/// one receiver).
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct UdpSection {
    pub pull: Vec<UdpPullEntry>,
    pub push: Vec<UdpPushEntry>,
}

/// One `[[udp.push]]`: send `stream` as MPEG-TS over UDP (`udp://10.0.0.20:5000`)
/// or RTP/MP2T (`rtp://...`) to one receiver's address while it is live: an
/// ATSC/DVB multiplexer's IP input, an IRD, a playout or another server.
/// A multicast address works too (then `[[multicast]]` has more options).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UdpPushEntry {
    pub stream: String,
    pub url: String,
    /// IP TTL: how many routers the datagrams may cross.
    #[serde(default = "default_udp_push_ttl")]
    pub ttl: u32,
    /// This host's IPv4 address on the video network: the source address
    /// of the datagrams, which picks the NIC. Absent: the OS's route.
    #[serde(default)]
    pub interface: Option<std::net::Ipv4Addr>,
    /// Pace datagrams to the stream's own clock (receivers with small
    /// buffers, and switches, drop bursts).
    #[serde(default = "default_true")]
    pub pacing: bool,
}

/// The usual unicast default (Linux, macOS, Windows).
fn default_udp_push_ttl() -> u32 {
    64
}

impl UdpPushEntry {
    pub fn to_target(&self) -> Result<caudal_multicast::MulticastTarget, String> {
        let ctx = |e: String| format!("[[udp.push]] `{}`: {e}", self.stream);
        let (format, rest) = if let Some(r) = self.url.strip_prefix("udp://") {
            (caudal_multicast::Format::Ts, r)
        } else if let Some(r) = self.url.strip_prefix("rtp://") {
            (caudal_multicast::Format::Rtp, r)
        } else {
            return Err(ctx(format!("url {:?}: expected udp://host:port or rtp://host:port", self.url)));
        };
        let rest = rest.trim_end_matches('/');
        let dest: std::net::SocketAddr = rest.parse().map_err(|_| {
            ctx(format!("url {:?}: expected an IP address and a port (udp://10.0.0.20:5000)", self.url))
        })?;
        let interface = match (self.interface, dest) {
            (None, _) => caudal_multicast::Interface::Default,
            (Some(a), std::net::SocketAddr::V4(_)) => caudal_multicast::Interface::V4(a),
            (Some(_), std::net::SocketAddr::V6(_)) => {
                return Err(ctx("`interface` is an IPv4 address; this destination is IPv6".into()));
            }
        };
        let target = caudal_multicast::MulticastTarget {
            stream: self.stream.clone(),
            group: dest,
            format,
            ttl: self.ttl,
            interface,
            loopback: false,
            pacing: self.pacing,
        };
        target.validate().map_err(ctx)?;
        Ok(target)
    }
}

/// `[http]`: only `[[http.pull]]` today.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct HttpSection {
    pub pull: Vec<HttpPullEntry>,
}

/// One `[[http.pull]]`: GET `url` (`http://10.0.0.5:8080/stream.ts`) and
/// publish the MPEG-TS body as `stream`; reconnects when it ends.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HttpPullEntry {
    pub stream: String,
    pub url: String,
    /// Start this pull only while someone watches: the first viewer's
    /// request starts it, and it stops `idle_secs` after the last viewer
    /// left. See `docs/ON-DEMAND.md`.
    #[serde(default)]
    pub on_demand: bool,
    /// With `on_demand`: seconds without viewers before the pull stops
    /// (default 30, 1..=3600).
    #[serde(default)]
    pub idle_secs: Option<u64>,
}

impl HttpSection {
    pub fn pull_targets(&self) -> Vec<caudal_http_pull::HttpPull> {
        self.pull
            .iter()
            .filter(|p| !p.on_demand)
            .map(|p| caudal_http_pull::HttpPull { stream: p.stream.clone(), url: p.url.clone() })
            .collect()
    }
}

/// One `[[udp.pull]]`: listen on `url` (`udp://239.1.1.1:5000`,
/// `udp://:5000`, `rtp://...`) and publish the transport stream as
/// `stream`. `interface`: the local IPv4 address to join a multicast group
/// on, when the machine has several.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UdpPullEntry {
    pub stream: String,
    pub url: String,
    #[serde(default)]
    pub interface: Option<std::net::Ipv4Addr>,
    /// Start this pull only while someone watches: the first viewer's
    /// request starts it, and it stops `idle_secs` after the last viewer
    /// left. See `docs/ON-DEMAND.md`.
    #[serde(default)]
    pub on_demand: bool,
    /// With `on_demand`: seconds without viewers before the pull stops
    /// (default 30, 1..=3600).
    #[serde(default)]
    pub idle_secs: Option<u64>,
}

impl UdpSection {
    pub fn pull_targets(&self) -> Vec<caudal_udp::UdpPull> {
        self.pull
            .iter()
            .filter(|p| !p.on_demand)
            .map(|p| caudal_udp::UdpPull { stream: p.stream.clone(), url: p.url.clone(), interface: p.interface })
            .collect()
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RestreamEntry {
    pub stream: String,
    pub url: String,
}

/// `[push]`: `[[push.auto]]` rules.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct PushSection {
    pub auto: Vec<AutoPushEntry>,
}

/// One `[[push.auto]]` rule (MistServer's "automatic push"): whenever a
/// stream matching one of `streams` goes live, push it to `url` with
/// `$stream` replaced by its name; stop when it ends. The URL's scheme
/// picks the engine, the same ones the static pushes use: `rtmp://` /
/// `rtmps://` (`[[restream]]`), `srt://` (`[[srt.push]]`), `udp://` /
/// `rtp://` (`[[udp.push]]`), `whip://` / `whip+http://` (`[[webrtc.push]]`).
#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AutoPushEntry {
    /// Stream names or patterns: exact (`main`), `prefix*` or `*`. Names
    /// with `+` (transcode renditions) never match, as in `[transcode]`.
    pub streams: Vec<String>,
    /// Target URL template. Variables: `$stream` / `${stream}` (the
    /// stream's name) and `$basename` / `${basename}` (the same, since
    /// `name+rendition` streams never match).
    pub url: String,
    /// `udp://` / `rtp://` only: as `[[udp.push]]` `ttl` (default 64).
    #[serde(default)]
    pub ttl: Option<u32>,
    /// `udp://` / `rtp://` only: as `[[udp.push]]` `interface`.
    #[serde(default)]
    pub interface: Option<std::net::Ipv4Addr>,
    /// `udp://` / `rtp://` only: as `[[udp.push]]` `pacing` (default on).
    #[serde(default)]
    pub pacing: Option<bool>,
    /// `whip://` / `whip+http://` only: as `[[webrtc.push]]` `token`.
    #[serde(default)]
    pub token: Option<String>,
    /// Schedule, one-off: RFC 3339 instant from which the rule pushes
    /// (MistServer's `scheduletime`). Without `stop`/`duration_secs` the
    /// window never closes.
    #[serde(default)]
    pub start: Option<String>,
    /// Schedule, one-off: RFC 3339 instant at which the rule stops pushing
    /// (MistServer's `completetime`). Without `start` the window is open
    /// from now until then.
    #[serde(default)]
    pub stop: Option<String>,
    /// Schedule, recurring: a 5-field crontab (`min hour dom month dow`),
    /// as in `[[record.schedule]]`; each window is `duration_secs` long.
    #[serde(default)]
    pub cron: Option<String>,
    /// IANA zone for `cron` (default UTC).
    #[serde(default)]
    pub tz: Option<String>,
    /// Window length: required with `cron`, or with `start` instead of
    /// `stop`.
    #[serde(default)]
    pub duration_secs: Option<u32>,
    /// `false` keeps the rule in the file but pushes nothing. The API can
    /// override it at runtime (`POST /api/v1/auto-pushes/{index}/enable`).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Stream patterns (as `streams`): while any live stream matches one,
    /// the rule pushes nothing. E.g. a backup feed pushed only while the
    /// main one is down.
    #[serde(default)]
    pub inhibit: Vec<String>,
}

/// Hand-written so `{:?}` never prints `token` in full.
impl std::fmt::Debug for AutoPushEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutoPushEntry")
            .field("streams", &self.streams)
            .field("url", &self.url)
            .field("ttl", &self.ttl)
            .field("interface", &self.interface)
            .field("pacing", &self.pacing)
            .field("token", &self.token.as_ref().map(|_| "****"))
            .field("start", &self.start)
            .field("stop", &self.stop)
            .field("cron", &self.cron)
            .field("tz", &self.tz)
            .field("duration_secs", &self.duration_secs)
            .field("enabled", &self.enabled)
            .field("inhibit", &self.inhibit)
            .finish()
    }
}

/// Which push engine an auto-push URL goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoPushKind {
    Restream,
    Srt,
    Udp,
    Whip,
}

impl AutoPushKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AutoPushKind::Restream => "rtmp",
            AutoPushKind::Srt => "srt",
            AutoPushKind::Udp => "udp",
            AutoPushKind::Whip => "whip",
        }
    }
}

/// Does `pattern` (exact, `prefix*` or `*`) select stream `name`? Names
/// with `+` are renditions and never match (the rule
/// `caudal-transcode` uses for its ladders).
pub fn stream_pattern_matches(pattern: &str, name: &str) -> bool {
    if name.contains('+') {
        return false;
    }
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pattern == name,
    }
}

/// Replaces MistServer-style target variables in `template` for `stream`.
/// Unknown `$names` are left as they are (a stream key may contain `$`).
pub fn expand_push_template(template: &str, stream: &str) -> String {
    // `name+rendition` streams never match an auto-push rule, so the
    // basename is the whole name.
    let basename = stream.split_once('+').map_or(stream, |(b, _)| b);
    template
        .replace("${stream}", stream)
        .replace("${basename}", basename)
        .replace("$stream", stream)
        .replace("$basename", basename)
}

impl AutoPushEntry {
    fn ctx(&self, e: impl std::fmt::Display) -> String {
        format!("[[push.auto]] {:?}: {e}", crate::config_edit::redact(&self.url))
    }

    pub fn kind(&self) -> Result<AutoPushKind, String> {
        match self.url.split_once("://").map(|(s, _)| s) {
            Some("rtmp" | "rtmps") => Ok(AutoPushKind::Restream),
            Some("srt") => Ok(AutoPushKind::Srt),
            Some("udp" | "rtp") => Ok(AutoPushKind::Udp),
            Some("whip" | "whip+http") => Ok(AutoPushKind::Whip),
            _ => Err(self.ctx("expected an rtmp://, rtmps://, srt://, udp://, rtp://, whip:// or whip+http:// url")),
        }
    }

    pub fn matches(&self, name: &str) -> bool {
        self.streams.iter().any(|p| stream_pattern_matches(p, name))
    }

    /// The first live stream in `live` that inhibits this rule.
    pub fn inhibited_by<'a>(&self, live: &'a [String]) -> Option<&'a String> {
        live.iter().find(|n| self.inhibit.iter().any(|p| stream_pattern_matches(p, n)))
    }

    /// The rule's time window, `None` when it has none (always on).
    pub fn timetable(&self) -> Result<Option<caudal_record::Timetable>, String> {
        let spec = caudal_record::TimetableConfig {
            start: self.start.clone(),
            stop: self.stop.clone(),
            cron: self.cron.clone(),
            tz: self.tz.clone(),
            duration_secs: self.duration_secs,
        };
        if spec == caudal_record::TimetableConfig::default() {
            return Ok(None);
        }
        if spec.tz.is_some() && spec.cron.is_none() {
            return Err(self.ctx("`tz` only applies to `cron` (`start`/`stop` carry their own offset)"));
        }
        caudal_record::Timetable::parse(&spec).map(Some).map_err(|e| self.ctx(e))
    }

    /// The UDP/RTP target for `stream`, through the same checks as
    /// `[[udp.push]]`.
    pub fn udp_target(&self, stream: &str) -> Result<caudal_multicast::MulticastTarget, String> {
        UdpPushEntry {
            stream: stream.to_owned(),
            url: expand_push_template(&self.url, stream),
            ttl: self.ttl.unwrap_or_else(default_udp_push_ttl),
            interface: self.interface,
            pacing: self.pacing.unwrap_or(true),
        }
        .to_target()
        .map_err(|e| self.ctx(e))
    }

    /// Checks the rule as far as possible without a live stream: valid
    /// patterns, a known scheme, and a template that expands to a usable
    /// URL for a sample stream name.
    pub fn validate(&self) -> Result<(), String> {
        if self.streams.is_empty() {
            return Err(self.ctx("`streams` is empty"));
        }
        self.timetable()?;
        for p in self.streams.iter().chain(&self.inhibit) {
            let concrete = p.strip_suffix('*').unwrap_or(p);
            if p != "*" && (concrete.contains('*') || !caudal_core::media::valid_stream_name(concrete)) {
                return Err(self.ctx(format!(
                    "stream pattern {p:?}: expected a stream name, `prefix*` or `*` \
                     (A-Z a-z 0-9 _ - ., at most 128)"
                )));
            }
            if concrete.contains('+') {
                return Err(
                    self.ctx(format!("stream pattern {p:?}: `name+rendition` streams never match an auto push"))
                );
            }
        }
        let kind = self.kind()?;
        if kind != AutoPushKind::Udp && (self.ttl.is_some() || self.interface.is_some() || self.pacing.is_some()) {
            return Err(self.ctx("`ttl`, `interface` and `pacing` only apply to udp:// and rtp:// targets"));
        }
        match (&self.token, kind) {
            (Some(t), AutoPushKind::Whip) => whip_token_ok("[[push.auto]]", t)?,
            (Some(_), _) => return Err(self.ctx("`token` only applies to whip:// and whip+http:// targets")),
            (None, _) => {}
        }
        let sample = self.sample_stream();
        let url = expand_push_template(&self.url, &sample);
        match kind {
            AutoPushKind::Udp => {
                if url != self.url {
                    return Err(
                        self.ctx("a udp:// or rtp:// target is an IP address and a port; it takes no variables")
                    );
                }
                self.udp_target(&sample)?;
            }
            AutoPushKind::Whip => {
                caudal_webrtc::whip_endpoint(&url).map_err(|e| self.ctx(format!("expands to {e}")))?;
            }
            AutoPushKind::Restream | AutoPushKind::Srt => {
                let parsed = url::Url::parse(&url)
                    .map_err(|e| self.ctx(format!("expands to {:?}: {e}", crate::config_edit::redact(&url))))?;
                if parsed.host_str().is_none_or(str::is_empty) {
                    return Err(
                        self.ctx(format!("expands to {:?}, which has no host", crate::config_edit::redact(&url)))
                    );
                }
                if kind == AutoPushKind::Srt && parsed.port().is_none() {
                    return Err(self.ctx(format!(
                        "expands to {:?}: an srt:// target needs a port",
                        crate::config_edit::redact(&url)
                    )));
                }
            }
        }
        Ok(())
    }

    /// A stream name the rule matches, for checking the template.
    fn sample_stream(&self) -> String {
        let p = &self.streams[0];
        match p.strip_suffix('*') {
            Some("") => "stream".into(),
            Some(prefix) => format!("{prefix}1"),
            None => p.clone(),
        }
    }
}

/// One multicast output: send `stream` to `group` (`239.1.1.1:5000`, or
/// `[ff15::1]:5000` for IPv6) while it is live.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MulticastEntry {
    pub stream: String,
    pub group: String,
    /// `ts` (MPEG-TS over UDP, the default) or `rtp` (RFC 2250, PT 33).
    #[serde(default = "default_multicast_format")]
    pub format: String,
    /// IP TTL / IPv6 hop limit.
    #[serde(default = "default_multicast_ttl")]
    pub ttl: u32,
    /// IPv4: the address of the interface to send on (also the source
    /// address). IPv6: the interface index. Absent: the OS's route.
    #[serde(default)]
    pub interface: Option<String>,
    /// Also deliver to receivers on this host (`IP_MULTICAST_LOOP`).
    #[serde(default)]
    pub loopback: bool,
    /// Pace datagrams to the stream's own clock (switches drop bursts).
    #[serde(default = "default_true")]
    pub pacing: bool,
}

fn default_multicast_format() -> String {
    "ts".into()
}

fn default_multicast_ttl() -> u32 {
    16
}

impl MulticastEntry {
    pub fn to_target(&self) -> Result<caudal_multicast::MulticastTarget, String> {
        let ctx = |e: String| format!("[[multicast]] `{}`: {e}", self.stream);
        let group: std::net::SocketAddr =
            self.group.parse().map_err(|_| ctx(format!("group {:?}: expected ip:port", self.group)))?;
        let format = match self.format.as_str() {
            "ts" => caudal_multicast::Format::Ts,
            "rtp" => caudal_multicast::Format::Rtp,
            other => return Err(ctx(format!("format {other:?}: expected \"ts\" or \"rtp\""))),
        };
        let interface = match (&self.interface, group) {
            (None, _) => caudal_multicast::Interface::Default,
            (Some(i), std::net::SocketAddr::V4(_)) => caudal_multicast::Interface::V4(
                i.parse().map_err(|_| ctx(format!("interface {i:?}: expected an IPv4 address")))?,
            ),
            (Some(i), std::net::SocketAddr::V6(_)) => caudal_multicast::Interface::V6Index(
                i.parse().map_err(|_| ctx(format!("interface {i:?}: expected an interface index")))?,
            ),
        };
        let target = caudal_multicast::MulticastTarget {
            stream: self.stream.clone(),
            group,
            format,
            ttl: self.ttl,
            interface,
            loopback: self.loopback,
            pacing: self.pacing,
        };
        if !group.ip().is_multicast() {
            return Err(ctx(format!(
                "group {group} is not a multicast address (IPv4 224.0.0.0/4, IPv6 ff00::/8); \
                 to send to one receiver's address use [[udp.push]]"
            )));
        }
        target.validate().map_err(ctx)?;
        Ok(target)
    }
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
    /// Start this pull only while someone watches: the first viewer's
    /// request starts it, and it stops `idle_secs` after the last viewer
    /// left. See `docs/ON-DEMAND.md`.
    #[serde(default)]
    pub on_demand: bool,
    /// With `on_demand`: seconds without viewers before the pull stops
    /// (default 30, 1..=3600).
    #[serde(default)]
    pub idle_secs: Option<u64>,
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
        self.pull
            .iter()
            .filter(|p| !p.on_demand)
            .map(|p| caudal_rtsp::RtspPull { stream: p.stream.clone(), url: p.url.clone() })
            .collect()
    }
}

impl SrtSection {
    /// `[[srt.pull]]` as the runtime type `caudal-srt` takes.
    pub fn pull_targets(&self) -> Vec<caudal_srt::SrtPull> {
        self.pull
            .iter()
            .filter(|p| !p.on_demand)
            .map(|p| caudal_srt::SrtPull { stream: p.stream.clone(), url: p.url.clone() })
            .collect()
    }

    /// `[[srt.push]]` as the runtime type `caudal-srt` takes.
    pub fn push_targets(&self) -> Vec<caudal_srt::SrtPush> {
        self.push.iter().map(|p| caudal_srt::SrtPush { stream: p.stream.clone(), url: p.url.clone() }).collect()
    }
}

/// `[omt]`: Open Media Transport (see `docs/OMT.md`). `[[omt.pull]]` receives
/// an OMT source into a stream (VMX decoded here, H.264/AAC by ffmpeg);
/// `[[omt.output]]` sends a stream out as an OMT source. Every pull and
/// output shares one discovery (mDNS responder and/or discovery server),
/// set up by the keys below.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct OmtSection {
    /// ffmpeg for the pulls' H.264/AAC encode. Absent: `[transcode] ffmpeg`.
    pub ffmpeg: Option<std::path::PathBuf>,
    /// Use this discovery server (`omt://host[:port]`, port 6399 by
    /// default) to find sources and announce outputs, for networks without
    /// multicast. Absent: mDNS (DNS-SD) only.
    pub discovery_server: Option<String>,
    /// mDNS on these interfaces only (names like `en0`/`eth1`, or an
    /// address on the interface). Empty: every non-loopback interface.
    pub interfaces: Vec<String>,
    /// Never use these interfaces for mDNS.
    pub exclude_interfaces: Vec<String>,
    pub pull: Vec<OmtPullEntry>,
    pub output: Vec<OmtOutputEntry>,
}

/// Quality suggested to the other side (`Default` defers to other
/// receivers / follows receivers' suggestions).
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OmtQuality {
    #[default]
    Default,
    Low,
    Medium,
    High,
}

impl OmtQuality {
    pub fn to_omt(self) -> open_media_transport::command::Quality {
        use open_media_transport::command::Quality;
        match self {
            OmtQuality::Default => Quality::Default,
            OmtQuality::Low => Quality::Low,
            OmtQuality::Medium => Quality::Medium,
            OmtQuality::High => Quality::High,
        }
    }
}

/// One `[[omt.pull]]`: receive an OMT source and publish it as `stream`.
/// Exactly one of `source` (the full name `MACHINE (Name)`, as `omt list`
/// and vMix/OBS show it) or `url` (`omt://host:port`).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OmtPullEntry {
    pub stream: String,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub quality: OmtQuality,
    /// H.264 bitrate of the published stream.
    #[serde(default = "default_omt_video_kbps")]
    pub video_kbps: u32,
    /// AAC bitrate of the published stream.
    #[serde(default = "default_audio_kbps")]
    pub audio_kbps: u32,
    /// Follow a redirect the source sends (OMT §9: "use this other source
    /// instead"). Off by default: anyone who can reach the sender's port
    /// could point it elsewhere and put their picture on Caudal's public
    /// outputs. Off: no redirect is followed; on: every one is (libomtnet's
    /// behaviour).
    #[serde(default)]
    pub follow_redirects: bool,
    /// Connect only to addresses inside these CIDRs (bare address = one
    /// host), whatever a name resolves to. Empty (the default): any. A `url`
    /// with a literal IP outside them is a config error; names are checked
    /// when resolved — NOT ENFORCED YET (see `docs/OMT.md`, Security).
    #[serde(default)]
    pub allowed_sources: Vec<String>,
}

fn default_omt_video_kbps() -> u32 {
    6000
}

/// One `[[omt.output]]`: send `stream` (H.264, any rendition) as the OMT
/// source `name`, announced as `MACHINE (name)`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OmtOutputEntry {
    pub stream: String,
    pub name: String,
    #[serde(default)]
    pub quality: OmtQuality,
    /// VMX encoder threads. 0 (the default): pick from the frame size, as
    /// libomtnet does.
    #[serde(default)]
    pub encoder_threads: usize,
    /// Listen only on this local address. Absent: every interface.
    #[serde(default)]
    pub bind: Option<std::net::IpAddr>,
    /// Accept receivers only from these CIDRs (bare address = one host).
    /// Empty (the default): any. NOT ENFORCED YET.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Most receiver connections at once (each video receiver costs one
    /// VMX send; audio and metadata ride on their own connections).
    /// Absent: the OMT library's default cap (128).
    #[serde(default)]
    pub max_connections: Option<usize>,
}

/// Security settings the config accepts and validates but the pinned OMT
/// library (rev ec11ec2) cannot apply yet: filtering what a name resolves
/// to on the receiver, and a peer allow-list on the sender. (Redirect
/// policy, bind and the connection cap are applied since ec11ec2.) Named
/// here so the startup log can say exactly which are inert.
pub fn omt_unenforced(section: &OmtSection) -> Vec<String> {
    let mut out = Vec::new();
    for p in &section.pull {
        if !p.allowed_sources.is_empty() && p.source.is_some() {
            out.push(format!("[[omt.pull]] `{}`: allowed_sources for a name (not checked when resolved)", p.stream));
        }
    }
    for o in &section.output {
        if !o.allow.is_empty() {
            out.push(format!("[[omt.output]] `{}`: allow (any receiver may connect)", o.name));
        }
    }
    out
}

const OMT_MAX_ENCODER_THREADS: usize = 64;
const OMT_MAX_CONNECTIONS: usize = 1000;

fn parse_cidrs(list: &[String]) -> Result<Vec<caudal_core::Cidr>, String> {
    list.iter().map(|s| caudal_core::Cidr::parse(s)).collect()
}

impl OmtSection {
    /// The ffmpeg the pulls run: `[omt] ffmpeg`, else `[transcode] ffmpeg`.
    pub fn ffmpeg<'a>(&'a self, transcode: &'a TranscodeSection) -> &'a std::path::Path {
        self.ffmpeg.as_deref().unwrap_or(&transcode.ffmpeg)
    }

    /// What the shared discovery is built from. Changing it needs a restart
    /// (every pull and output holds the one discovery).
    pub fn discovery_config(&self) -> open_media_transport::discovery::DiscoveryConfig {
        open_media_transport::discovery::DiscoveryConfig {
            server: self.discovery_server.clone(),
            browse_mdns: true,
            interfaces: open_media_transport::discovery::Interfaces {
                only: self.interfaces.clone(),
                exclude: self.exclude_interfaces.clone(),
            },
        }
    }

    /// The part of `[omt]` a reload cannot apply live.
    pub fn discovery_key(&self) -> (&Option<String>, &Vec<String>, &Vec<String>) {
        (&self.discovery_server, &self.interfaces, &self.exclude_interfaces)
    }

    /// Whether any pull or output needs the shared discovery: pulls by full
    /// name (to look it up) and every output (to announce it).
    pub fn needs_discovery(&self) -> bool {
        !self.output.is_empty() || self.pull.iter().any(|p| p.source.is_some())
    }

    /// Checks `[omt]`; `taken` lists streams other ingests already publish.
    pub fn validate(&self, taken: &[(&str, String)]) -> Result<(), String> {
        if let Some(url) = &self.discovery_server {
            open_media_transport::discovery_server::parse_url(url)
                .map_err(|e| format!("[omt] discovery_server {url:?}: {e} (expected omt://host[:port])"))?;
        }
        for i in self.interfaces.iter().chain(&self.exclude_interfaces) {
            if i.trim().is_empty() {
                return Err("[omt] interfaces/exclude_interfaces: empty interface name".into());
            }
        }
        let mut seen = std::collections::HashSet::new();
        for p in &self.pull {
            let ctx = |e: String| format!("[[omt.pull]] `{}`: {e}", p.stream);
            if !caudal_core::media::valid_stream_name(&p.stream) {
                return Err(format!("[[omt.pull]] stream `{}` is not a valid stream name", p.stream));
            }
            if !seen.insert(p.stream.as_str()) {
                return Err(format!("[[omt.pull]] duplicate stream `{}`", p.stream));
            }
            if let Some((_, what)) = taken.iter().find(|(s, _)| *s == p.stream) {
                return Err(ctx(format!("stream is also {what}")));
            }
            match (&p.source, &p.url) {
                (Some(_), Some(_)) => return Err(ctx("set either `source` or `url`, not both".into())),
                (None, None) => {
                    return Err(ctx("needs `source` (\"MACHINE (Name)\") or `url` (omt://host:port)".into()));
                }
                (Some(s), None) => {
                    if !open_media_transport::discovery::is_valid_full_name(s)
                        || open_media_transport::discovery::split_full_name(s).is_none()
                    {
                        return Err(ctx(format!(
                            "source {s:?}: expected a full name \"MACHINE (Name)\" (use `url` for omt://host:port)"
                        )));
                    }
                }
                (None, Some(u)) => match open_media_transport::address::Address::parse(u) {
                    Ok(open_media_transport::address::Address::Url { .. }) => {}
                    Ok(_) => return Err(ctx(format!("url {u:?}: expected omt://host:port"))),
                    Err(e) => return Err(ctx(format!("url {u:?}: {e}"))),
                },
            }
            if !(100..=200_000).contains(&p.video_kbps) {
                return Err(ctx(format!("video_kbps {} is outside 100..=200000", p.video_kbps)));
            }
            if !(16..=1024).contains(&p.audio_kbps) {
                return Err(ctx(format!("audio_kbps {} is outside 16..=1024", p.audio_kbps)));
            }
            let allowed = parse_cidrs(&p.allowed_sources).map_err(|e| ctx(format!("allowed_sources: {e}")))?;
            if let (Some(u), false) = (&p.url, allowed.is_empty())
                && let Ok(open_media_transport::address::Address::Url { host, .. }) =
                    open_media_transport::address::Address::parse(u)
                && let Ok(ip) = host.parse::<std::net::IpAddr>()
                && !allowed.iter().any(|c| c.contains(ip))
            {
                return Err(ctx(format!("url {u:?} is outside allowed_sources")));
            }
        }
        let mut names = std::collections::HashSet::new();
        for o in &self.output {
            let ctx = |e: String| format!("[[omt.output]] `{}`: {e}", o.name);
            if !caudal_core::media::valid_stream_name(&o.stream) {
                return Err(ctx(format!("stream `{}` is not a valid stream name", o.stream)));
            }
            let n = o.name.trim();
            if n.is_empty() {
                return Err("[[omt.output]] `name` must not be empty".into());
            }
            if n != o.name || o.name.len() > 128 || o.name.chars().any(|c| c.is_control() || c == '(' || c == ')') {
                return Err(ctx(
                    "name must be 1-128 characters without parentheses, control characters or edge spaces (it is announced as \"MACHINE (name)\")".into(),
                ));
            }
            if !names.insert(o.name.as_str()) {
                return Err(format!("[[omt.output]] duplicate name `{}`", o.name));
            }
            if o.encoder_threads > OMT_MAX_ENCODER_THREADS {
                return Err(ctx(format!("encoder_threads {} is over {OMT_MAX_ENCODER_THREADS}", o.encoder_threads)));
            }
            parse_cidrs(&o.allow).map_err(|e| ctx(format!("allow: {e}")))?;
            if let Some(n) = o.max_connections
                && !(1..=OMT_MAX_CONNECTIONS).contains(&n)
            {
                return Err(ctx(format!("max_connections {n} is outside 1..={OMT_MAX_CONNECTIONS}")));
            }
        }
        Ok(())
    }

    /// `[[omt.pull]]` as the runtime type `caudal-omt` takes.
    pub fn pull_configs(
        &self,
        transcode: &TranscodeSection,
        directory: Option<std::sync::Arc<open_media_transport::address::Directory>>,
    ) -> Vec<caudal_omt::PullConfig> {
        self.pull
            .iter()
            .map(|p| caudal_omt::PullConfig {
                stream: p.stream.clone(),
                source: p.source.clone().or_else(|| p.url.clone()).expect("validated"),
                quality: p.quality.to_omt(),
                video_kbps: p.video_kbps,
                audio_kbps: p.audio_kbps,
                ffmpeg: self.ffmpeg(transcode).to_path_buf(),
                directory: directory.clone(),
                redirects: if p.follow_redirects {
                    open_media_transport::receiver::RedirectPolicy::Any
                } else {
                    open_media_transport::receiver::RedirectPolicy::Never
                },
            })
            .collect()
    }

    /// What every output shares: the ffmpeg used for what the in-process
    /// decoders cannot do (H.264 other than 8-bit 4:2:0, AAC other than LC).
    pub fn output_options(&self, transcode: &TranscodeSection) -> caudal_omt::OutputOptions {
        caudal_omt::OutputOptions { ffmpeg: Some(self.ffmpeg(transcode).to_path_buf()), decode_with_ffmpeg: false }
    }

    /// `[[omt.output]]` as the runtime type `caudal-omt` takes.
    pub fn output_configs(
        &self,
        discovery: Option<std::sync::Arc<open_media_transport::discovery::Discovery>>,
    ) -> Vec<caudal_omt::OutputConfig> {
        self.output
            .iter()
            .map(|o| caudal_omt::OutputConfig {
                stream: o.stream.clone(),
                name: o.name.clone(),
                quality: o.quality.to_omt(),
                max_quality: None,
                encoder_threads: o.encoder_threads,
                announce: true,
                discovery: discovery.clone(),
                bind: o.bind,
                max_connections: o.max_connections,
            })
            .collect()
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

/// WHIP ingest and WHEP playback over one UDP port, and WHIP pushes out.
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
    /// `[[webrtc.push]]`: send streams to remote WHIP endpoints. Hot
    /// reloadable, unlike the rest of the section.
    pub push: Vec<WhipPushEntry>,
}

impl Default for WebRtcSection {
    fn default() -> Self {
        Self { udp_bind: default_webrtc_udp(), public_ips: Vec::new(), threads: 0, push: Vec::new() }
    }
}

impl WebRtcSection {
    /// Whether the part wired at startup (the UDP port and its engines)
    /// changed; `push` reloads on its own.
    pub fn listener_changed(&self, other: &Self) -> bool {
        (self.udp_bind, &self.public_ips, self.threads) != (other.udp_bind, &other.public_ips, other.threads)
    }

    pub fn push_targets(&self) -> Vec<caudal_webrtc::WhipPush> {
        self.push
            .iter()
            .map(|p| caudal_webrtc::WhipPush { stream: p.stream.clone(), url: p.url.clone(), token: p.token.clone() })
            .collect()
    }
}

/// One `[[webrtc.push]]`: while `stream` is live, publish it to the WHIP
/// endpoint `url` (`whip://` = HTTPS, `whip+http://` = plain HTTP, or an
/// `https://` / `http://` URL as is).
#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WhipPushEntry {
    pub stream: String,
    pub url: String,
    /// Sent as `Authorization: Bearer <token>`.
    #[serde(default)]
    pub token: Option<String>,
}

impl std::fmt::Debug for WhipPushEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WhipPushEntry")
            .field("stream", &self.stream)
            .field("url", &caudal_webrtc::redact_whip_url(&self.url))
            .field("token", &self.token.as_ref().map(|_| "****"))
            .finish()
    }
}

/// A bearer token: printable ASCII without spaces (it goes in a header).
pub(crate) fn whip_token_ok(ctx: &str, token: &str) -> Result<(), String> {
    if token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(format!("{ctx}: `token` must be non-empty printable ASCII without spaces"));
    }
    Ok(())
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
#[derive(Clone, Deserialize, PartialEq, Eq)]
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
    /// Blocking authorization webhook, asked after the token check.
    pub hook: Option<AuthHookSection>,
    /// Blocking routing webhook, asked before everything else but stream
    /// keys.
    pub rewrite: Option<AuthRewriteSection>,
    /// Where to pull a stream nobody publishes and nothing configures
    /// (MistServer's `STREAM_SOURCE`), asked when a viewer requests it.
    pub source: Option<AuthSourceSection>,
    /// `[[auth.stream_keys]]`: secrets an encoder publishes with in place
    /// of the stream name. See `docs/STREAM-KEYS.md`.
    pub stream_keys: Vec<StreamKeySection>,
    /// A TOML file of more `[[stream_keys]]`, read at start and on every
    /// reload (SIGHUP, `POST /api/v1/config/reload`). A relative path is
    /// relative to the working directory.
    pub stream_keys_file: Option<std::path::PathBuf>,
    /// A stream with a publish key refuses a publish that came without
    /// it (a token or the bare name). Default false.
    pub stream_keys_only: bool,
    /// The most tokens signed with `secret` may grant.
    pub secret_permissions: Option<KeyPermissionsSection>,
    /// The most tokens signed with a JWKS key may grant, by `kid`.
    pub key_permissions: Vec<KidPermissionsSection>,
    /// The keys read from `stream_keys_file` (see `Config::read_external`);
    /// part of `PartialEq`, so a reload sees a changed file.
    #[serde(skip)]
    pub file_stream_keys: Vec<StreamKeySection>,
}

/// One stream key. `Debug` never prints the key.
#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StreamKeySection {
    pub key: String,
    pub stream: String,
    #[serde(default = "default_true")]
    pub publish: bool,
    #[serde(default)]
    pub play: bool,
}

impl std::fmt::Debug for StreamKeySection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamKeySection")
            .field("key", &"****")
            .field("stream", &self.stream)
            .field("publish", &self.publish)
            .field("play", &self.play)
            .finish()
    }
}

/// The shape of `[auth] stream_keys_file`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct StreamKeysFile {
    stream_keys: Vec<StreamKeySection>,
}

/// `[auth.secret_permissions]`: MistServer's per-key `input`/`output`/
/// `stream`. Defaults allow everything, so only what is set narrows.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct KeyPermissionsSection {
    #[serde(default = "default_true")]
    pub publish: bool,
    #[serde(default = "default_true")]
    pub play: bool,
    /// Exact names or `prefix*`; empty: any stream.
    #[serde(default)]
    pub streams: Vec<String>,
}

/// `[[auth.key_permissions]]`: [`KeyPermissionsSection`] for one JWKS `kid`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct KidPermissionsSection {
    pub kid: String,
    #[serde(default = "default_true")]
    pub publish: bool,
    #[serde(default = "default_true")]
    pub play: bool,
    #[serde(default)]
    pub streams: Vec<String>,
}

fn key_permissions(
    what: &str,
    publish: bool,
    play: bool,
    streams: &[String],
) -> Result<caudal_auth::KeyPermissions, String> {
    if !publish && !play {
        return Err(format!("{what}: `publish` and `play` are both false; remove the key instead"));
    }
    for p in streams {
        let concrete = p.strip_suffix('*').unwrap_or(p);
        if concrete.contains('*') || (!concrete.is_empty() && !caudal_core::media::valid_stream_name(concrete)) {
            return Err(format!("{what}: `{p}` is not a stream name or a `prefix*` pattern"));
        }
    }
    Ok(caudal_auth::KeyPermissions { publish, play, streams: streams.to_vec() })
}

/// Hand-written so `{:?}` never prints `secret` in full.
impl std::fmt::Debug for AuthSection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthSection")
            .field("secret", &self.secret.as_ref().map(|_| "****"))
            .field("jwks_url", &self.jwks_url)
            .field("jwks_refresh_secs", &self.jwks_refresh_secs)
            .field("publish", &self.publish)
            .field("play", &self.play)
            .field("hook", &self.hook)
            .field("rewrite", &self.rewrite)
            .finish()
    }
}

impl Default for AuthSection {
    fn default() -> Self {
        Self {
            secret: None,
            jwks_url: None,
            jwks_refresh_secs: default_jwks_refresh_secs(),
            publish: true,
            play: false,
            hook: None,
            rewrite: None,
            source: None,
            stream_keys: Vec::new(),
            stream_keys_file: None,
            stream_keys_only: false,
            secret_permissions: None,
            key_permissions: Vec::new(),
            file_stream_keys: Vec::new(),
        }
    }
}

/// `[auth.hook]`: before a publish or a play goes ahead, POST it (signed,
/// Standard Webhooks) to `url` and wait for the answer. 2xx allows
/// (with `reply = "mistserver"`, only when the body reads as true), 401/403
/// deny; a timeout or any other answer is decided by `on_error`.
/// See `caudal_auth::GateHook` and `docs/AUTH-HOOK.md`.
#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuthHookSection {
    pub url: String,
    /// `whsec_...`, as in `[hooks]`.
    pub secret: String,
    /// One attempt, no retries. 100..=10000.
    #[serde(default = "default_auth_hook_timeout_ms")]
    pub timeout_ms: u64,
    /// `"deny"` (default) or `"allow"` when the service fails.
    #[serde(default = "default_auth_hook_on_error")]
    pub on_error: String,
    /// How a 2xx is read: `"status"` (default, any 2xx allows) or
    /// `"mistserver"` (the body must read as true, as a MistServer
    /// blocking trigger handler answers).
    #[serde(default = "default_auth_hook_reply")]
    pub reply: String,
    #[serde(default = "default_true")]
    pub publish: bool,
    #[serde(default = "default_true")]
    pub play: bool,
    /// How long an allow is remembered for the same stream, IP and token.
    #[serde(default = "default_auth_hook_allow_cache_secs")]
    pub allow_cache_secs: u64,
    /// How long a denial or a failure is remembered.
    #[serde(default = "default_auth_hook_deny_cache_secs")]
    pub deny_cache_secs: u64,
}

/// Hand-written so `{:?}` never prints `secret` or `url` in full.
impl std::fmt::Debug for AuthHookSection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthHookSection")
            .field("url", &caudal_core::net::redact_url_host(&self.url))
            .field("secret", &"****")
            .field("timeout_ms", &self.timeout_ms)
            .field("on_error", &self.on_error)
            .field("reply", &self.reply)
            .field("publish", &self.publish)
            .field("play", &self.play)
            .field("allow_cache_secs", &self.allow_cache_secs)
            .field("deny_cache_secs", &self.deny_cache_secs)
            .finish()
    }
}

fn default_auth_hook_timeout_ms() -> u64 {
    2000
}
fn default_auth_hook_on_error() -> String {
    "deny".into()
}
fn default_auth_hook_reply() -> String {
    "status".into()
}
fn default_auth_hook_allow_cache_secs() -> u64 {
    30
}
fn default_auth_hook_deny_cache_secs() -> u64 {
    5
}

impl AuthHookSection {
    pub fn to_config(&self) -> Result<caudal_auth::GateHookConfig, String> {
        let url = reqwest_url_ok(&self.url).ok_or_else(|| {
            format!("[auth.hook] url {:?}: expected http(s)://", caudal_core::net::redact_url_host(&self.url))
        })?;
        if !self.secret.starts_with("whsec_") {
            return Err("[auth.hook] `secret` must be a Standard Webhooks secret (whsec_...)".into());
        }
        if !(100..=10_000).contains(&self.timeout_ms) {
            return Err(format!("[auth.hook] timeout_ms {} is outside 100..=10000", self.timeout_ms));
        }
        let on_error = match self.on_error.as_str() {
            "deny" => caudal_auth::OnError::Deny,
            "allow" => caudal_auth::OnError::Allow,
            other => return Err(format!("[auth.hook] on_error {other:?}: expected \"deny\" or \"allow\"")),
        };
        let reply = match self.reply.as_str() {
            "status" => caudal_auth::ReplyMode::Status,
            "mistserver" => caudal_auth::ReplyMode::MistServer,
            other => return Err(format!("[auth.hook] reply {other:?}: expected \"status\" or \"mistserver\"")),
        };
        if !self.publish && !self.play {
            return Err("[auth.hook] `publish` and `play` are both false; remove the section instead".into());
        }
        if self.allow_cache_secs > 3600 || self.deny_cache_secs > 3600 {
            return Err("[auth.hook] cache times are at most 3600 s".into());
        }
        Ok(caudal_auth::GateHookConfig {
            url,
            secret: self.secret.clone(),
            timeout: Duration::from_millis(self.timeout_ms),
            on_error,
            reply,
            publish: self.publish,
            play: self.play,
            allow_cache: Duration::from_secs(self.allow_cache_secs),
            deny_cache: Duration::from_secs(self.deny_cache_secs),
        })
    }
}

/// `[auth.rewrite]`: before a publish or a play, POST it (signed, Standard
/// Webhooks) to `url` and use the stream name the answer gives (MistServer's
/// `PUSH_REWRITE` / `PLAY_REWRITE`). An empty answer or 401/403 refuses; a
/// timeout or any other answer is decided by `on_error` (`allow` keeps the
/// requested name). See `caudal_auth::RewriteHook` and `docs/REWRITE-HOOK.md`.
///
/// It sits under `[auth]` rather than `[hooks]` because it behaves like
/// `[auth.hook]` (blocking, fail-closed by default, reloaded with `[auth]`),
/// while `[hooks]` only notifies, after the fact, and never blocks.
#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuthRewriteSection {
    pub url: String,
    /// `whsec_...`, as in `[hooks]`.
    pub secret: String,
    /// One attempt, no retries. 100..=10000.
    #[serde(default = "default_auth_hook_timeout_ms")]
    pub timeout_ms: u64,
    /// `"deny"` (default) or `"allow"` (keep the requested name) when the
    /// service fails.
    #[serde(default = "default_auth_hook_on_error")]
    pub on_error: String,
    /// Route publishes (`PUSH_REWRITE`).
    #[serde(default = "default_true")]
    pub publish: bool,
    /// Route plays (`PLAY_REWRITE`).
    #[serde(default = "default_true")]
    pub play: bool,
    /// How long an answer is remembered for the same protocol, stream, IP
    /// and token; the service's `cache_secs` can only lower it.
    #[serde(default = "default_auth_hook_allow_cache_secs")]
    pub cache_secs: u64,
    /// How long a denial or a failure is remembered.
    #[serde(default = "default_auth_hook_deny_cache_secs")]
    pub deny_cache_secs: u64,
}

/// Hand-written so `{:?}` never prints `secret` or `url` in full.
impl std::fmt::Debug for AuthRewriteSection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthRewriteSection")
            .field("url", &caudal_core::net::redact_url_host(&self.url))
            .field("secret", &"****")
            .field("timeout_ms", &self.timeout_ms)
            .field("on_error", &self.on_error)
            .field("publish", &self.publish)
            .field("play", &self.play)
            .field("cache_secs", &self.cache_secs)
            .field("deny_cache_secs", &self.deny_cache_secs)
            .finish()
    }
}

impl AuthRewriteSection {
    pub fn to_config(&self) -> Result<caudal_auth::RewriteHookConfig, String> {
        let url = reqwest_url_ok(&self.url).ok_or_else(|| {
            format!("[auth.rewrite] url {:?}: expected http(s)://", caudal_core::net::redact_url_host(&self.url))
        })?;
        if !self.secret.starts_with("whsec_") {
            return Err("[auth.rewrite] `secret` must be a Standard Webhooks secret (whsec_...)".into());
        }
        if !(100..=10_000).contains(&self.timeout_ms) {
            return Err(format!("[auth.rewrite] timeout_ms {} is outside 100..=10000", self.timeout_ms));
        }
        let on_error = match self.on_error.as_str() {
            "deny" => caudal_auth::OnError::Deny,
            "allow" => caudal_auth::OnError::Allow,
            other => return Err(format!("[auth.rewrite] on_error {other:?}: expected \"deny\" or \"allow\"")),
        };
        if !self.publish && !self.play {
            return Err("[auth.rewrite] `publish` and `play` are both false; remove the section instead".into());
        }
        if self.cache_secs > 3600 || self.deny_cache_secs > 3600 {
            return Err("[auth.rewrite] cache times are at most 3600 s".into());
        }
        Ok(caudal_auth::RewriteHookConfig {
            url,
            secret: self.secret.clone(),
            timeout: Duration::from_millis(self.timeout_ms),
            on_error,
            publish: self.publish,
            play: self.play,
            cache: Duration::from_secs(self.cache_secs),
            deny_cache: Duration::from_secs(self.deny_cache_secs),
        })
    }
}

/// `[auth.source]`: when a viewer asks for a stream that is neither live
/// nor configured, POST its name (signed, Standard Webhooks) to `url`; the
/// answer is a source URL (`srt://`, `udp://`, `rtp://`, `http(s)://` TS or
/// `.m3u8`, `rtmp(s)://`, `rtsp(s)://`) that Caudal pulls on demand until
/// `idle_secs` after the last viewer left, or empty for "no such stream".
/// MistServer's `STREAM_SOURCE`. See `caudal_auth::SourceHook` and
/// `docs/ON-DEMAND.md`.
///
/// Under `[auth]` with `[auth.hook]` and `[auth.rewrite]`: it blocks the
/// request, reuses their signing, timeout and cache, and reloads with them.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuthSourceSection {
    pub url: String,
    /// `whsec_...`, as in `[hooks]`.
    pub secret: String,
    /// One attempt, no retries. 100..=10000.
    #[serde(default = "default_auth_hook_timeout_ms")]
    pub timeout_ms: u64,
    /// How long a source is remembered for a stream name; the service's
    /// `cache_secs` can only lower it.
    #[serde(default = "default_auth_source_cache_secs")]
    pub cache_secs: u64,
    /// How long "no such stream" or a failure is remembered.
    #[serde(default = "default_auth_hook_deny_cache_secs")]
    pub deny_cache_secs: u64,
    /// Seconds without viewers before a pull it started stops.
    #[serde(default = "default_idle_secs")]
    pub idle_secs: u64,
}

fn default_auth_source_cache_secs() -> u64 {
    10
}

fn default_idle_secs() -> u64 {
    crate::on_demand::DEFAULT_IDLE.as_secs()
}

impl AuthSourceSection {
    pub fn to_config(&self) -> Result<caudal_auth::SourceHookConfig, String> {
        let url = reqwest_url_ok(&self.url)
            .ok_or_else(|| format!("[auth.source] url {:?}: expected http(s)://", self.url))?;
        if !self.secret.starts_with("whsec_") {
            return Err("[auth.source] `secret` must be a Standard Webhooks secret (whsec_...)".into());
        }
        if !(100..=10_000).contains(&self.timeout_ms) {
            return Err(format!("[auth.source] timeout_ms {} is outside 100..=10000", self.timeout_ms));
        }
        if self.cache_secs > 3600 || self.deny_cache_secs > 3600 {
            return Err("[auth.source] cache times are at most 3600 s".into());
        }
        if !(1..=3600).contains(&self.idle_secs) {
            return Err(format!("[auth.source] idle_secs {} is outside 1..=3600", self.idle_secs));
        }
        Ok(caudal_auth::SourceHookConfig {
            url,
            secret: self.secret.clone(),
            timeout: Duration::from_millis(self.timeout_ms),
            cache: Duration::from_secs(self.cache_secs),
            deny_cache: Duration::from_secs(self.deny_cache_secs),
        })
    }
}

/// `Some(url)` for an absolute http(s) URL with a host.
fn reqwest_url_ok(s: &str) -> Option<String> {
    let rest = s.strip_prefix("https://").or_else(|| s.strip_prefix("http://"))?;
    let host = rest.split(['/', '?', '#']).next()?;
    (!host.is_empty() && !host.contains(char::is_whitespace)).then(|| s.to_owned())
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
        let secret_permissions = match &self.secret_permissions {
            Some(_) if self.secret.is_none() => {
                return Err("[auth.secret_permissions] needs `secret`".into());
            }
            Some(p) => Some(key_permissions("[auth.secret_permissions]", p.publish, p.play, &p.streams)?),
            None => None,
        };
        if !self.key_permissions.is_empty() && self.jwks_url.is_none() {
            return Err("[[auth.key_permissions]] needs `jwks_url`".into());
        }
        let mut kid_permissions = std::collections::HashMap::new();
        for k in &self.key_permissions {
            if k.kid.is_empty() {
                return Err("[[auth.key_permissions]] needs a `kid`".into());
            }
            let what = format!("[[auth.key_permissions]] kid `{}`", k.kid);
            let perms = key_permissions(&what, k.publish, k.play, &k.streams)?;
            if kid_permissions.insert(k.kid.clone(), perms).is_some() {
                return Err(format!("{what} is listed twice"));
            }
        }
        Ok(caudal_auth::AuthConfig {
            keys,
            publish: self.publish,
            play: self.play,
            secret_permissions,
            kid_permissions,
        })
    }

    /// Every stream key, from `[[auth.stream_keys]]` then the file,
    /// validated. Errors name the stream, never the key.
    pub fn stream_keys(&self) -> Result<Vec<caudal_auth::StreamKey>, String> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::with_capacity(self.stream_keys.len() + self.file_stream_keys.len());
        for (from, k) in self
            .stream_keys
            .iter()
            .map(|k| ("[[auth.stream_keys]]", k))
            .chain(self.file_stream_keys.iter().map(|k| ("stream_keys_file", k)))
        {
            let what = format!("{from} for `{}`", k.stream);
            caudal_auth::validate_key(&k.key).map_err(|e| format!("{what}: {e}"))?;
            if !caudal_core::media::valid_stream_name(&k.stream) {
                return Err(format!("{what}: not a valid stream name"));
            }
            if !k.publish && !k.play {
                return Err(format!("{what}: `publish` and `play` are both false"));
            }
            if !seen.insert(k.key.as_str()) {
                return Err(format!("{what}: the same key is listed twice"));
            }
            out.push(caudal_auth::StreamKey {
                key: k.key.clone(),
                stream: k.stream.clone(),
                publish: k.publish,
                play: k.play,
            });
        }
        if self.stream_keys_only && out.is_empty() {
            return Err("[auth] stream_keys_only needs at least one stream key".into());
        }
        Ok(out)
    }

    /// Reads `stream_keys_file` into `file_stream_keys`.
    fn read_stream_keys_file(&mut self) -> Result<(), String> {
        let Some(path) = &self.stream_keys_file else {
            self.file_stream_keys.clear();
            return Ok(());
        };
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("[auth] stream_keys_file {}: {e}", path.display()))?;
        // `toml`'s error quotes the offending line, which may hold a key.
        let file: StreamKeysFile = toml::from_str(&text)
            .map_err(|_| format!("[auth] stream_keys_file {}: not a valid `[[stream_keys]]` file", path.display()))?;
        self.file_stream_keys = file.stream_keys;
        Ok(())
    }

    /// `[auth.hook]` as the runtime config, `None` when absent.
    pub fn hook_config(&self) -> Result<Option<caudal_auth::GateHookConfig>, String> {
        self.hook.as_ref().map(AuthHookSection::to_config).transpose()
    }

    /// `[auth.rewrite]` as the runtime config, `None` when absent.
    pub fn rewrite_config(&self) -> Result<Option<caudal_auth::RewriteHookConfig>, String> {
        self.rewrite.as_ref().map(AuthRewriteSection::to_config).transpose()
    }

    /// `[auth.source]` as the runtime config, `None` when absent.
    pub fn source_config(&self) -> Result<Option<caudal_auth::SourceHookConfig>, String> {
        self.source.as_ref().map(AuthSourceSection::to_config).transpose()
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
#[derive(Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct HooksSection {
    pub urls: Vec<String>,
    /// `whsec_...`
    pub secret: Option<String>,
}

/// Hand-written so `{:?}` never prints `secret` or `urls` in full.
impl std::fmt::Debug for HooksSection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HooksSection")
            .field("urls", &self.urls.iter().map(|u| caudal_core::net::redact_url_host(u)).collect::<Vec<_>>())
            .field("secret", &self.secret.as_ref().map(|_| "****"))
            .finish()
    }
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
#[derive(Clone, Deserialize, PartialEq)]
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

/// Hand-written so `{:?}` never prints `secret` or `webhooks` in full.
impl std::fmt::Debug for HealthSection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HealthSection")
            .field("no_keyframe_secs", &self.no_keyframe_secs)
            .field("min_bitrate_kbps", &self.min_bitrate_kbps)
            .field("min_bitrate_for_secs", &self.min_bitrate_for_secs)
            .field("no_audio_secs", &self.no_audio_secs)
            .field("publisher_lost", &self.publisher_lost)
            .field("publisher_lost_grace_secs", &self.publisher_lost_grace_secs)
            .field("min_hold_secs", &self.min_hold_secs)
            .field("webhooks", &self.webhooks.iter().map(|u| caudal_core::net::redact_url_host(u)).collect::<Vec<_>>())
            .field("secret", &self.secret.as_ref().map(|_| "****"))
            .field("streams", &self.streams)
            .finish()
    }
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
        self.auth.stream_keys()?;
        self.auth.hook_config()?;
        self.auth.rewrite_config()?;
        self.auth.source_config()?;
        self.access.to_access_config()?;
        self.server.trusted_proxy_cidrs()?;
        self.hooks.to_hooks_config()?;
        self.moq.to_moq_config(self.buffer.to_buffer_config())?;
        self.record.to_record_config()?;
        self.transcode.to_transcode_config(self.buffer.to_buffer_config())?;
        self.rtsp.to_tls_config()?;
        self.rtmp.tls_source(&self.tls)?;
        self.validate_pulls_and_pushes()?;
        if let Some(admin) = &self.admin {
            admin.validate()?;
        }
        self.health.to_health_config()?;
        self.captions.validate(self.hls.segment_ms)?;
        if let Some(c) = &self.cluster {
            c.edge_config(self.buffer.to_buffer_config())?;
            if c.role == ClusterRole::Origin && !self.moq.enabled {
                return Err(r#"[cluster] role = "origin" needs [moq] enabled (edges pull over MoQ)"#.into());
            }
        }
        self.failovers()?;
        let static_udp = self.multicast_targets()?;
        self.validate_auto_pushes(&static_udp)?;
        self.omt.validate(&self.omt_taken_streams())?;
        Ok(())
    }

    /// `[[push.auto]]`: each rule on its own, then UDP destinations: an
    /// auto rule may not send to a destination a `[[multicast]]`,
    /// `[[udp.push]]` or another rule already sends to (one stream per
    /// destination).
    fn validate_auto_pushes(&self, static_udp: &[caudal_multicast::MulticastTarget]) -> Result<(), String> {
        let mut seen: std::collections::HashSet<std::net::SocketAddr> = static_udp.iter().map(|t| t.group).collect();
        for rule in &self.push.auto {
            rule.validate()?;
            if rule.kind()? == AutoPushKind::Udp {
                let group = rule.udp_target(&rule.sample_stream())?.group;
                if !seen.insert(group) {
                    return Err(rule.ctx(format!(
                        "UDP destination {group} is already used by [[multicast]], [[udp.push]] or another \
                         [[push.auto]] rule: one stream per destination"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Streams another ingest already publishes, which an `[[omt.pull]]`
    /// must not also publish: `(stream, "a [[channel]]")`.
    fn omt_taken_streams(&self) -> Vec<(&str, String)> {
        let mut taken: Vec<(&str, String)> = Vec::new();
        taken.extend(self.channel.iter().map(|c| (c.name.as_str(), "a [[channel]]".to_string())));
        taken.extend(self.failover.iter().map(|f| (f.stream.as_str(), "a [[failover]] stream".to_string())));
        taken.extend(self.rtsp.pull.iter().map(|p| (p.stream.as_str(), "an [[rtsp.pull]]".to_string())));
        taken
    }

    /// `[[multicast]]` and `[[udp.push]]` as the runtime type
    /// `caudal-multicast` takes (one sender for both), checked: parseable,
    /// multicast groups for `[[multicast]]`, one output per destination.
    pub fn multicast_targets(&self) -> Result<Vec<caudal_multicast::MulticastTarget>, String> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::with_capacity(self.multicast.len() + self.udp.push.len());
        let targets = self
            .multicast
            .iter()
            .map(MulticastEntry::to_target)
            .chain(self.udp.push.iter().map(UdpPushEntry::to_target));
        for t in targets {
            let t = t?;
            if !seen.insert(t.group) {
                return Err(format!(
                    "UDP destination {} is used twice ([[multicast]] / [[udp.push]]): one stream per destination",
                    t.group
                ));
            }
            out.push(t);
        }
        Ok(out)
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

impl Config {
    /// Every `[[*.pull]]` except `[[omt.pull]]`: section, stream, url,
    /// `on_demand`, `idle_secs`.
    fn pull_specs(&self) -> Vec<(&'static str, &str, &str, bool, Option<u64>)> {
        let mut v: Vec<(&'static str, &str, &str, bool, Option<u64>)> = Vec::new();
        v.extend(self.rtsp.pull.iter().map(|p| ("[[rtsp.pull]]", &*p.stream, &*p.url, p.on_demand, p.idle_secs)));
        v.extend(self.srt.pull.iter().map(|p| ("[[srt.pull]]", &*p.stream, &*p.url, p.on_demand, p.idle_secs)));
        v.extend(self.udp.pull.iter().map(|p| ("[[udp.pull]]", &*p.stream, &*p.url, p.on_demand, p.idle_secs)));
        v.extend(self.http.pull.iter().map(|p| ("[[http.pull]]", &*p.stream, &*p.url, p.on_demand, p.idle_secs)));
        v.extend(self.rtmp.pull.iter().map(|p| ("[[rtmp.pull]]", &*p.stream, &*p.url, p.on_demand, p.idle_secs)));
        v.extend(self.hls.pull.iter().map(|p| ("[[hls.pull]]", &*p.stream, &*p.url, p.on_demand, p.idle_secs)));
        v
    }

    /// The `on_demand = true` pull entries, by stream name.
    pub fn on_demand_entries(&self) -> std::collections::HashMap<String, crate::on_demand::Entry> {
        use crate::on_demand::{Entry, Source};
        let idle = |i: Option<u64>| i.map_or(crate::on_demand::DEFAULT_IDLE, Duration::from_secs);
        let mut out = std::collections::HashMap::new();
        let mut put = |stream: &str, source: Source, i: Option<u64>| {
            out.insert(stream.to_owned(), Entry { source, idle: idle(i) });
        };
        for p in self.rtsp.pull.iter().filter(|p| p.on_demand) {
            let src = Source::Rtsp(caudal_rtsp::RtspPull { stream: p.stream.clone(), url: p.url.clone() });
            put(&p.stream, src, p.idle_secs);
        }
        for p in self.srt.pull.iter().filter(|p| p.on_demand) {
            let src = Source::Srt(caudal_srt::SrtPull { stream: p.stream.clone(), url: p.url.clone() });
            put(&p.stream, src, p.idle_secs);
        }
        for p in self.udp.pull.iter().filter(|p| p.on_demand) {
            let src = Source::Udp(caudal_udp::UdpPull {
                stream: p.stream.clone(),
                url: p.url.clone(),
                interface: p.interface,
            });
            put(&p.stream, src, p.idle_secs);
        }
        for p in self.http.pull.iter().filter(|p| p.on_demand) {
            let src = Source::Http(caudal_http_pull::HttpPull { stream: p.stream.clone(), url: p.url.clone() });
            put(&p.stream, src, p.idle_secs);
        }
        for p in self.rtmp.pull.iter().filter(|p| p.on_demand) {
            let src = Source::Rtmp(caudal_rtmp::RtmpPull { stream: p.stream.clone(), url: p.url.clone() });
            put(&p.stream, src, p.idle_secs);
        }
        for p in self.hls.pull.iter().filter(|p| p.on_demand) {
            let src = Source::Hls(caudal_hls_pull::HlsPull { stream: p.stream.clone(), url: p.url.clone() });
            put(&p.stream, src, p.idle_secs);
        }
        out
    }

    /// Every `[[*.pull]]`, `[[srt.push]]` and `[[restream]]`: a valid stream
    /// name, the right URL scheme, and one pull per stream (two would race
    /// to publish the same name).
    fn validate_pulls_and_pushes(&self) -> Result<(), String> {
        let name_ok = |section: &str, s: &str| {
            if caudal_core::media::valid_stream_name(s) {
                Ok(())
            } else {
                Err(format!("{section} stream {s:?} is not a valid stream name (A-Z a-z 0-9 _ - . +, at most 128)"))
            }
        };
        let scheme_ok = |section: &str, url: &str, schemes: &[&str]| {
            let ok = url.split_once("://").is_some_and(|(s, rest)| schemes.contains(&s) && !rest.is_empty());
            if ok {
                Ok(())
            } else {
                Err(format!(
                    "{section} url {:?}: expected {}",
                    crate::config_edit::redact(url),
                    schemes.join("://, ") + "://"
                ))
            }
        };
        let mut pulled = std::collections::HashSet::new();
        let pulls = self
            .rtsp
            .pull
            .iter()
            .map(|p| ("[[rtsp.pull]]", &p.stream, &p.url, &["rtsp", "rtsps"][..]))
            .chain(self.srt.pull.iter().map(|p| ("[[srt.pull]]", &p.stream, &p.url, &["srt"][..])))
            .chain(self.udp.pull.iter().map(|p| ("[[udp.pull]]", &p.stream, &p.url, &["udp", "rtp"][..])))
            .chain(self.http.pull.iter().map(|p| ("[[http.pull]]", &p.stream, &p.url, &["http", "https"][..])))
            .chain(self.rtmp.pull.iter().map(|p| ("[[rtmp.pull]]", &p.stream, &p.url, &["rtmp", "rtmps"][..])))
            .chain(self.hls.pull.iter().map(|p| ("[[hls.pull]]", &p.stream, &p.url, &["http", "https"][..])));
        for (section, stream, _, on_demand, idle) in self.pull_specs() {
            if idle.is_some() && !on_demand {
                return Err(format!("{section} stream {stream:?}: `idle_secs` needs `on_demand = true`"));
            }
            if let Some(i) = idle
                && !(1..=3600).contains(&i)
            {
                return Err(format!("{section} stream {stream:?}: idle_secs {i} is outside 1..=3600"));
            }
        }
        for (section, stream, url, schemes) in pulls {
            name_ok(section, stream)?;
            scheme_ok(section, url, schemes)?;
            if !pulled.insert(stream.as_str()) {
                return Err(format!("{section} stream {stream:?} is pulled twice"));
            }
        }
        for p in &self.http.pull {
            caudal_http_pull::parse_url(&p.url).map_err(|e| format!("[[http.pull]] url {e}"))?;
        }
        for p in &self.rtmp.pull {
            caudal_rtmp::pull::parse_url(&p.url).map_err(|e| format!("[[rtmp.pull]] url {e}"))?;
        }
        for p in &self.hls.pull {
            caudal_hls_pull::parse_url(&p.url).map_err(|e| format!("[[hls.pull]] url {e}"))?;
        }
        let mut listening = std::collections::HashSet::new();
        for p in &self.udp.pull {
            let t = caudal_udp::parse_url(&p.url).map_err(|e| format!("[[udp.pull]] url {e}"))?;
            if p.interface.is_some() && !t.is_multicast() {
                return Err(format!("[[udp.pull]] {:?}: `interface` only applies to a multicast group", p.url));
            }
            // Two pulls on one unicast port would split its datagrams.
            if !t.is_multicast() && !listening.insert(t.addr.port()) {
                return Err(format!("[[udp.pull]] port {} is used by two unicast pulls", t.addr.port()));
            }
        }
        for p in &self.srt.push {
            name_ok("[[srt.push]]", &p.stream)?;
            scheme_ok("[[srt.push]]", &p.url, &["srt"])?;
            caudal_srt::check_caller_url(&p.url).map_err(|e| format!("[[srt.push]] url {e}"))?;
        }
        for p in &self.srt.pull {
            caudal_srt::check_caller_url(&p.url).map_err(|e| format!("[[srt.pull]] url {e}"))?;
        }
        for p in &self.webrtc.push {
            name_ok("[[webrtc.push]]", &p.stream)?;
            caudal_webrtc::whip_endpoint(&p.url).map_err(|e| format!("[[webrtc.push]] url {e}"))?;
            if let Some(t) = &p.token {
                whip_token_ok("[[webrtc.push]]", t)?;
            }
        }
        for r in &self.restream {
            name_ok("[[restream]]", &r.stream)?;
            scheme_ok("[[restream]]", &r.url, &["rtmp", "rtmps"])?;
        }
        Ok(())
    }
}

/// Loads and validates a config file, rejecting unknown keys. The error
/// message names the offending key and its line number (from `toml`'s
/// span-aware `Display` impl).
pub fn load(path: &Path) -> Result<Config, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let mut cfg: Config = toml::from_str(&text).map_err(|e| e.to_string())?;
    cfg.read_external()?;
    cfg.validate()?;
    Ok(cfg)
}

impl Config {
    /// Reads the files the config points at whose contents are part of the
    /// config itself (`[auth] stream_keys_file`). Called by every path that
    /// turns TOML text into a running config, before `validate`.
    pub fn read_external(&mut self) -> Result<(), String> {
        self.auth.read_stream_keys_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `caudal.example.toml` as shipped, then with each commented example
    /// block uncommented in turn: every one must parse and validate, so the
    /// example never shows a key that was renamed or removed. A block is a
    /// run of commented TOML lines (`# [table]`, `# key = ...`, `# # alt`,
    /// or a `#   # note` continuation); a prose comment, a bare `#` or a blank
    /// line ends it. A block that needs another one (RTMPS needs `[tls]`'s
    /// certificate) is uncommented together with it. Placeholders such as
    /// `$argon2id$...` are filled in first, as a reader would.
    #[test]
    fn example_config_blocks_each_validate() {
        const EXAMPLE: &str = include_str!("../../../caudal.example.toml");
        fn is_toml_line(rest: &str) -> bool {
            let t = rest.trim_start();
            if t.starts_with('#') {
                return true; // `# # alt = ...`, or an aligned continuation of the comment above
            }
            if rest.starts_with(' ') {
                return false;
            }
            let header = t.starts_with('[')
                && t.rsplit_once(']').is_some_and(|(_, after)| {
                    let after = after.trim();
                    after.is_empty() || after.starts_with('#')
                });
            header
                || t.split_once(" = ").is_some_and(|(k, _)| {
                    !k.is_empty() && k.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                })
        }
        let lines: Vec<&str> = EXAMPLE.lines().collect();
        let toml_line = |l: &str| l.strip_prefix("# ").is_some_and(is_toml_line);
        let mut blocks: Vec<(usize, usize)> = Vec::new();
        let mut i = 0;
        while i < lines.len() {
            if toml_line(lines[i]) {
                let start = i;
                while i < lines.len() && toml_line(lines[i]) {
                    i += 1;
                }
                blocks.push((start, i));
            } else {
                i += 1;
            }
        }
        let uncomment = |ranges: &[(usize, usize)]| -> String {
            lines
                .iter()
                .enumerate()
                .map(|(n, l)| if ranges.iter().any(|&(a, b)| (a..b).contains(&n)) { &l[2..] } else { l })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let block_with =
            |needle: &str| *blocks.iter().find(|&&(a, b)| lines[a..b].iter().any(|l| l.contains(needle))).unwrap();
        // Blocks that only make sense together with another one.
        let needs = [
            ("# tls_bind = \"0.0.0.0:1936\"", "# [tls]"),
            ("# [[access.rules]]", "# [access]"),
            ("# [auth.secret_permissions]", "# [auth]"),
        ];
        // Placeholders the reader replaces, with values of the right shape.
        let phc = caudal_admin::hash_password("example passphrase").unwrap();
        let placeholders =
            [("$argon2id$v=19$m=19456,t=2,p=1$...", phc.as_str()), ("<64 hex characters>", &"ab".repeat(32))];
        let check = |what: &str, text: &str| {
            let mut text = text.to_owned();
            for (placeholder, value) in &placeholders {
                text = text.replace(placeholder, value);
            }
            let cfg: Config = toml::from_str(&text).unwrap_or_else(|e| panic!("{what}: does not parse: {e}"));
            if let Err(e) = cfg.validate() {
                // Features compiled out reject their section with a clear
                // message; a file the example names (the GeoIP database) is
                // not on this machine.
                // Not found: os error 2 on Unix; on Windows 2 (file) or 3
                // (a directory of the path is missing, as `/etc/caudal/`).
                let missing_file = e.contains("(os error 2)") || (cfg!(windows) && e.contains("(os error 3)"));
                assert!(e.contains(crate::captions::NOT_BUILT) || missing_file, "{what}: does not validate: {e}");
            }
        };
        check("the file as shipped", EXAMPLE);
        assert!(blocks.len() > 30, "found only {} example blocks: the splitter broke", blocks.len());
        for &(a, b) in &blocks {
            let mut ranges = vec![(a, b)];
            for (dependent, dependency) in needs {
                if lines[a..b].iter().any(|l| l.contains(dependent)) {
                    ranges.push(block_with(dependency));
                }
            }
            check(&format!("lines {}-{} uncommented ({})", a + 1, b, lines[a].trim()), &uncomment(&ranges));
        }
    }

    #[test]
    fn multicast_entries() {
        let cfg: Config = toml::from_str(
            "[[multicast]]\nstream = \"tv1\"\ngroup = \"239.1.1.1:5000\"\n\
             [[multicast]]\nstream = \"tv2\"\ngroup = \"[ff15::1]:5000\"\nformat = \"rtp\"\nttl = 1\n\
             interface = \"3\"\nloopback = true\npacing = false\n",
        )
        .unwrap();
        cfg.validate().unwrap();
        let t = cfg.multicast_targets().unwrap();
        assert_eq!(t[0].format, caudal_multicast::Format::Ts);
        assert_eq!((t[0].ttl, t[0].loopback, t[0].pacing), (16, false, true), "defaults");
        assert_eq!(t[0].interface, caudal_multicast::Interface::Default);
        assert_eq!(t[1].format, caudal_multicast::Format::Rtp);
        assert_eq!(t[1].interface, caudal_multicast::Interface::V6Index(3));
        assert_eq!((t[1].ttl, t[1].loopback, t[1].pacing), (1, true, false));

        let bad = |t: &str| toml::from_str::<Config>(t).unwrap().validate().unwrap_err();
        assert!(bad("[[multicast]]\nstream = \"a\"\ngroup = \"10.0.0.1:5000\"\n").contains("not a multicast"));
        assert!(bad("[[multicast]]\nstream = \"a\"\ngroup = \"239.1.1.1\"\n").contains("ip:port"));
        assert!(bad("[[multicast]]\nstream = \"a\"\ngroup = \"239.1.1.1:1\"\nformat = \"srt\"\n").contains("format"));
        assert!(
            bad("[[multicast]]\nstream = \"a\"\ngroup = \"239.1.1.1:1\"\ninterface = \"eth0\"\n")
                .contains("IPv4 address")
        );
        assert!(
            bad("[[multicast]]\nstream = \"a\"\ngroup = \"239.1.1.1:1\"\n[[multicast]]\nstream = \"b\"\ngroup = \"239.1.1.1:1\"\n")
                .contains("used twice")
        );
    }

    #[test]
    fn omt_section_parses_with_defaults() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.omt, OmtSection::default());
        assert!(!cfg.omt.needs_discovery());

        let src = r#"
[transcode]
ffmpeg = "/opt/ffmpeg/bin/ffmpeg"

[omt]
discovery_server = "omt://10.0.0.2"
interfaces = ["en0"]
exclude_interfaces = ["192.168.64.1"]

[[omt.pull]]
stream = "cam1"
source = "STUDIO-PC (Camera 1)"
quality = "high"

[[omt.pull]]
stream = "cam2"
url = "omt://10.0.0.7:6400"
video_kbps = 12000
audio_kbps = 192

[[omt.output]]
stream = "program"
name = "Caudal Program"

[[omt.output]]
stream = "program+720p"
name = "Caudal 720p"
quality = "medium"
encoder_threads = 4
"#;
        let cfg: Config = toml::from_str(src).unwrap();
        cfg.validate().unwrap();
        let omt = &cfg.omt;
        assert_eq!(omt.ffmpeg(&cfg.transcode), Path::new("/opt/ffmpeg/bin/ffmpeg"), "falls back to [transcode]");
        assert!(omt.needs_discovery());
        let d = omt.discovery_config();
        assert_eq!(d.server.as_deref(), Some("omt://10.0.0.2"));
        assert_eq!(d.interfaces.only, vec!["en0".to_string()]);
        assert_eq!(d.interfaces.exclude, vec!["192.168.64.1".to_string()]);

        let pulls = omt.pull_configs(&cfg.transcode, None);
        assert_eq!(pulls[0].source, "STUDIO-PC (Camera 1)");
        assert_eq!(pulls[0].quality, open_media_transport::command::Quality::High);
        assert_eq!((pulls[0].video_kbps, pulls[0].audio_kbps), (6000, 128), "defaults");
        assert_eq!(pulls[1].source, "omt://10.0.0.7:6400");
        assert_eq!(pulls[1].quality, open_media_transport::command::Quality::Default);
        assert_eq!((pulls[1].video_kbps, pulls[1].audio_kbps), (12000, 192));
        assert_eq!(pulls[1].ffmpeg, Path::new("/opt/ffmpeg/bin/ffmpeg"));

        let outputs = omt.output_configs(None);
        assert_eq!((outputs[0].stream.as_str(), outputs[0].name.as_str()), ("program", "Caudal Program"));
        assert_eq!(outputs[0].encoder_threads, 0, "0 = auto");
        assert_eq!(outputs[1].quality, open_media_transport::command::Quality::Medium);
        assert_eq!(outputs[1].encoder_threads, 4);

        let own: Config = toml::from_str("[omt]\nffmpeg = \"/usr/bin/ffmpeg\"\n").unwrap();
        assert_eq!(own.omt.ffmpeg(&own.transcode), Path::new("/usr/bin/ffmpeg"));

        let url_only: Config = toml::from_str("[[omt.pull]]\nstream = \"a\"\nurl = \"omt://h:1\"\n").unwrap();
        assert!(!url_only.omt.needs_discovery(), "a pull by URL needs no discovery");

        // Security keys: safe defaults, and the ones the library cannot
        // apply yet are named (never silently accepted).
        assert!(!cfg.omt.pull[0].follow_redirects, "redirects off by default");
        assert!(cfg.omt.pull[0].allowed_sources.is_empty());
        assert_eq!((cfg.omt.output[0].bind, cfg.omt.output[0].max_connections), (None, None));
        let secure: Config = toml::from_str(
            "[[omt.pull]]\nstream = \"a\"\nsource = \"M (N)\"\nfollow_redirects = true\nallowed_sources = [\"10.0.0.0/8\"]\n\
             [[omt.output]]\nstream = \"b\"\nname = \"B\"\nbind = \"10.0.0.1\"\nallow = [\"10.0.0.0/8\"]\nmax_connections = 4\n",
        )
        .unwrap();
        secure.validate().unwrap();
        let inert = omt_unenforced(&secure.omt);
        assert_eq!(inert.len(), 2, "only name filtering and the output allow-list are inert: {inert:?}");
        assert!(
            inert
                .iter()
                .all(|s| !s.contains("follow_redirects") && !s.contains("bind") && !s.contains("max_connections"))
        );
        assert!(omt_unenforced(&cfg.omt).is_empty(), "the defaults are all enforced");
        let pulls = secure.omt.pull_configs(&secure.transcode, None);
        assert_eq!(pulls[0].redirects, open_media_transport::receiver::RedirectPolicy::Any);
        assert_eq!(
            cfg.omt.pull_configs(&cfg.transcode, None)[0].redirects,
            open_media_transport::receiver::RedirectPolicy::Never
        );
        let outs = secure.omt.output_configs(None);
        assert_eq!((outs[0].bind, outs[0].max_connections), (Some("10.0.0.1".parse().unwrap()), Some(4)));
    }

    #[test]
    fn omt_section_rejects_bad_entries() {
        let bad = |t: &str| toml::from_str::<Config>(t).unwrap().validate().unwrap_err();
        let pull = |extra: &str| format!("[[omt.pull]]\nstream = \"cam\"\n{extra}");

        assert!(bad(&pull("source = \"A (B)\"\nurl = \"omt://h:1\"\n")).contains("not both"));
        assert!(bad(&pull("")).contains("needs `source`"));
        assert!(bad(&pull("source = \"just-a-host\"\n")).contains("MACHINE (Name)"));
        assert!(bad(&pull("url = \"omt://host\"\n")).contains("url"), "a pull URL needs a port");
        assert!(bad(&pull("url = \"STUDIO (Cam)\"\n")).contains("omt://host:port"));
        assert!(bad(&pull("url = \"omt://h:1\"\nvideo_kbps = 10\n")).contains("video_kbps"));
        assert!(bad(&pull("url = \"omt://h:1\"\naudio_kbps = 0\n")).contains("audio_kbps"));
        assert!(bad("[[omt.pull]]\nstream = \"a b\"\nurl = \"omt://h:1\"\n").contains("not a valid stream name"));
        let twice = "[[omt.pull]]\nstream = \"cam\"\nurl = \"omt://h:1\"\n[[omt.pull]]\nstream = \"cam\"\nurl = \"omt://h:2\"\n";
        assert!(bad(twice).contains("duplicate stream `cam`"));
        let channel = "[[channel]]\nname = \"cam\"\nitems = []\n[[omt.pull]]\nstream = \"cam\"\nurl = \"omt://h:1\"\n";
        assert!(bad(channel).contains("also a [[channel]]"));
        let rtsp = "[[rtsp.pull]]\nstream = \"cam\"\nurl = \"rtsp://x/y\"\n[[omt.pull]]\nstream = \"cam\"\nurl = \"omt://h:1\"\n";
        assert!(bad(rtsp).contains("[[rtsp.pull]]"));
        let quality = toml::from_str::<Config>(&pull("url = \"omt://h:1\"\nquality = \"ultra\"\n")).unwrap_err();
        assert!(quality.to_string().contains("ultra"), "{quality}");
        let typo = toml::from_str::<Config>(&pull("url = \"omt://h:1\"\nvideo_kbs = 1\n")).unwrap_err();
        assert!(typo.to_string().contains("video_kbs"), "unknown keys rejected: {typo}");

        let out = |extra: &str| format!("[[omt.output]]\nstream = \"show\"\n{extra}");
        assert!(toml::from_str::<Config>(&out("")).is_err(), "name is required");
        assert!(bad(&out("name = \"\"\n")).contains("must not be empty"));
        assert!(bad(&out("name = \"Show (A)\"\n")).contains("parentheses"));
        assert!(bad(&out("name = \" Show\"\n")).contains("edge spaces"));
        assert!(bad(&out("name = \"Show\"\nencoder_threads = 65\n")).contains("encoder_threads"));
        let dup = "[[omt.output]]\nstream = \"a\"\nname = \"Show\"\n[[omt.output]]\nstream = \"b\"\nname = \"Show\"\n";
        assert!(bad(dup).contains("duplicate name `Show`"));
        // The same stream under two names is fine.
        let two = "[[omt.output]]\nstream = \"a\"\nname = \"One\"\n[[omt.output]]\nstream = \"a\"\nname = \"Two\"\n";
        toml::from_str::<Config>(two).unwrap().validate().unwrap();

        assert!(
            bad(&pull("url = \"omt://10.0.0.7:6400\"\nallowed_sources = [\"192.168.1.0/24\"]\n"))
                .contains("outside allowed_sources")
        );
        assert!(bad(&pull("url = \"omt://h:1\"\nallowed_sources = [\"nope\"]\n")).contains("allowed_sources"));
        toml::from_str::<Config>(&pull("url = \"omt://10.0.0.7:6400\"\nallowed_sources = [\"10.0.0.0/8\"]\n"))
            .unwrap()
            .validate()
            .unwrap();
        assert!(bad(&out("name = \"S\"\nallow = [\"300.1.1.1\"]\n")).contains("allow"));
        assert!(bad(&out("name = \"S\"\nmax_connections = 0\n")).contains("max_connections"));
        assert!(toml::from_str::<Config>(&out("name = \"S\"\nbind = \"eth0\"\n")).is_err(), "bind is an address");

        assert!(bad("[omt]\ndiscovery_server = \"http://x\"\n").contains("discovery_server"));
        assert!(bad("[omt]\ninterfaces = [\" \"]\n").contains("empty interface"));
        let ok: Config = toml::from_str("[omt]\ndiscovery_server = \"omt://[::1]:6399\"\n").unwrap();
        ok.validate().unwrap();
    }

    #[test]
    fn defaults_when_empty() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.server.http_bind, default_http_bind());
        assert_eq!(cfg.rtmp.bind, default_rtmp_bind());
        assert_eq!(cfg.rtmp.app, "live");
        assert_eq!(cfg.hls.part_ms, 200);
        assert_eq!(cfg.hls.segment_ms, 2000);
        assert_eq!(cfg.buffer.window_secs, 15);
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
        assert!(HlsSection::default().ts && toml::from_str::<Config>("").unwrap().hls.ts, "ts-hls defaults on");
        let cfg: Config = toml::from_str("[hls]\nts = false\n").unwrap();
        assert!(!cfg.hls.ts);
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
    fn rtmps_bind_change_restarts_the_listener() {
        let a = RtmpSection::default();
        let b = RtmpSection { tls_bind: Some("0.0.0.0:1936".parse().unwrap()), ..RtmpSection::default() };
        assert!(a.listener_changed(&b), "turning RTMPS on must restart the listener");
        let c = RtmpSection {
            pull: vec![RtmpPullEntry {
                stream: "x".into(),
                url: "rtmp://h/a/k".into(),
                on_demand: false,
                idle_secs: None,
            }],
            ..RtmpSection::default()
        };
        assert!(!a.listener_changed(&c), "a pull change alone does not");
    }

    #[test]
    fn http_pulls_are_validated() {
        let ok: Config = toml::from_str(
            "[[http.pull]]\nstream = \"web\"\nurl = \"http://10.0.0.5:8080/stream.ts\"\n\
             [[http.pull]]\nstream = \"enc\"\nurl = \"https://encoder.example/live\"\n",
        )
        .unwrap();
        ok.validate().unwrap();
        assert_eq!(ok.http.pull_targets()[1].url, "https://encoder.example/live");
        let bad = |t: &str| toml::from_str::<Config>(t).unwrap().validate().unwrap_err();
        assert!(bad("[[http.pull]]\nstream = \"a\"\nurl = \"udp://h:1\"\n").contains("http://"));
        assert!(bad("[[http.pull]]\nstream = \"a\"\nurl = \"http://h/live/index.m3u8\"\n").contains("[[hls.pull]]"));
        let twice = "[[http.pull]]\nstream = \"a\"\nurl = \"http://h/x\"\n[[udp.pull]]\nstream = \"a\"\nurl = \"udp://:5000\"\n";
        assert!(bad(twice).contains("pulled twice"), "one pull per stream across protocols");
    }

    #[test]
    fn rtmp_pulls_are_validated() {
        let ok: Config = toml::from_str(
            "[rtmp]\n[[rtmp.pull]]\nstream = \"remote\"\nurl = \"rtmp://10.0.0.5/live/cam1\"\n\
             [[rtmp.pull]]\nstream = \"cdn\"\nurl = \"rtmps://live.example:443/app/KEY\"\n",
        )
        .unwrap();
        ok.validate().unwrap();
        assert_eq!(ok.rtmp.pull_targets()[1].url, "rtmps://live.example:443/app/KEY");
        assert!(!ok.rtmp.listener_changed(&RtmpSection::default()), "pulls are not the listener");
        let bad = |t: &str| toml::from_str::<Config>(t).unwrap().validate().unwrap_err();
        assert!(bad("[[rtmp.pull]]\nstream = \"a\"\nurl = \"srt://h:1\"\n").contains("rtmp://"));
        assert!(bad("[[rtmp.pull]]\nstream = \"a\"\nurl = \"rtmp://h/\"\n").contains("stream key"));
        let e = bad("[[rtmp.pull]]\nstream = \"a\"\nurl = \"rtmp://h:x/live/SECRET\"\n");
        assert!(!e.contains("SECRET"), "{e}");
        let twice = "[[rtmp.pull]]\nstream = \"a\"\nurl = \"rtmp://h/live/k\"\n\
                     [[http.pull]]\nstream = \"a\"\nurl = \"http://h/x\"\n";
        assert!(bad(twice).contains("pulled twice"), "one pull per stream across protocols");
    }

    #[test]
    fn hls_pulls_are_validated() {
        let ok: Config = toml::from_str(
            "[hls]\npart_ms = 100\n[[hls.pull]]\nstream = \"tv\"\nurl = \"https://cdn.example/live/master.m3u8\"\n",
        )
        .unwrap();
        ok.validate().unwrap();
        assert_eq!(ok.hls.part_ms, 100, "pulls sit beside the output settings");
        assert_eq!(ok.hls.pull_targets()[0].stream, "tv");
        assert_eq!(ok.hls.output(), HlsSection { part_ms: 100, ..HlsSection::default() });
        let bad = |t: &str| toml::from_str::<Config>(t).unwrap().validate().unwrap_err();
        assert!(bad("[[hls.pull]]\nstream = \"a\"\nurl = \"rtmp://h/x.m3u8\"\n").contains("http://"));
        assert!(bad("[[hls.pull]]\nstream = \"bad name\"\nurl = \"http://h/x.m3u8\"\n").contains("stream name"));
        let twice = "[[hls.pull]]\nstream = \"a\"\nurl = \"http://h/x.m3u8\"\n[[http.pull]]\nstream = \"a\"\nurl = \"http://h/x.ts\"\n";
        assert!(bad(twice).contains("pulled twice"), "one pull per stream across protocols");
    }

    #[test]
    fn udp_pulls_are_validated() {
        let ok: Config = toml::from_str(
            "[[udp.pull]]\nstream = \"tv\"\nurl = \"udp://239.1.1.1:5000\"\ninterface = \"10.0.0.2\"\n\
             [[udp.pull]]\nstream = \"cam\"\nurl = \"rtp://:5004\"\n",
        )
        .unwrap();
        ok.validate().unwrap();
        assert_eq!(ok.udp.pull_targets()[0].interface, Some("10.0.0.2".parse().unwrap()));
        let bad = |t: &str| toml::from_str::<Config>(t).unwrap().validate().unwrap_err();
        assert!(bad("[[udp.pull]]\nstream = \"a\"\nurl = \"udp://239.1.1.1\"\n").contains("port"));
        assert!(
            bad("[[udp.pull]]\nstream = \"a\"\nurl = \"udp://:5000\"\ninterface = \"10.0.0.2\"\n")
                .contains("multicast")
        );
        let same_port = "[[udp.pull]]\nstream = \"a\"\nurl = \"udp://:5000\"\n[[udp.pull]]\nstream = \"b\"\nurl = \"udp://127.0.0.1:5000\"\n";
        assert!(bad(same_port).contains("two unicast pulls"));
        let twice =
            "[[udp.pull]]\nstream = \"a\"\nurl = \"udp://:5000\"\n[[srt.pull]]\nstream = \"a\"\nurl = \"srt://h:1\"\n";
        assert!(bad(twice).contains("pulled twice"), "one pull per stream across protocols");
    }

    #[test]
    fn auth_hook_section() {
        let secret = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";
        let ok: Config =
            toml::from_str(&format!("[auth.hook]\nurl = \"https://auth.example/gate\"\nsecret = \"{secret}\"\n"))
                .unwrap();
        ok.validate().unwrap();
        let h = ok.auth.hook_config().unwrap().unwrap();
        assert_eq!(h.timeout, Duration::from_millis(2000), "short default timeout");
        assert_eq!(h.on_error, caudal_auth::OnError::Deny, "fails closed by default");
        assert_eq!(h.reply, caudal_auth::ReplyMode::Status, "status codes decide by default");
        assert!(h.publish && h.play);
        assert_eq!((h.allow_cache, h.deny_cache), (Duration::from_secs(30), Duration::from_secs(5)));
        assert!(Config::default().auth.hook_config().unwrap().is_none());

        let bad = |extra: &str| {
            toml::from_str::<Config>(&format!("[auth.hook]\nurl = \"https://a/g\"\nsecret = \"{secret}\"\n{extra}"))
                .unwrap()
                .validate()
                .unwrap_err()
        };
        assert!(bad("timeout_ms = 50\n").contains("timeout_ms"));
        assert!(bad("timeout_ms = 30000\n").contains("timeout_ms"));
        assert!(bad("on_error = \"maybe\"\n").contains("on_error"));
        assert!(bad("publish = false\nplay = false\n").contains("both false"));
        assert!(bad("allow_cache_secs = 86400\n").contains("3600"));
        assert!(bad("reply = \"body\"\n").contains("reply"));
        let mist: Config = toml::from_str(&format!(
            "[auth.hook]\nurl = \"https://a/g\"\nsecret = \"{secret}\"\nreply = \"mistserver\"\n"
        ))
        .unwrap();
        assert_eq!(mist.auth.hook_config().unwrap().unwrap().reply, caudal_auth::ReplyMode::MistServer);
        let err = |t: &str| toml::from_str::<Config>(t).unwrap().validate().unwrap_err();
        assert!(err("[auth.hook]\nurl = \"auth.example\"\nsecret = \"whsec_x\"\n").contains("http(s)"));
        assert!(err("[auth.hook]\nurl = \"https://a/g\"\nsecret = \"hunter2\"\n").contains("whsec_"));
    }

    #[test]
    fn auth_rewrite_section() {
        let secret = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";
        let ok: Config =
            toml::from_str(&format!("[auth.rewrite]\nurl = \"https://route.example/r\"\nsecret = \"{secret}\"\n"))
                .unwrap();
        ok.validate().unwrap();
        let r = ok.auth.rewrite_config().unwrap().unwrap();
        assert_eq!(r.timeout, Duration::from_millis(2000));
        assert_eq!(r.on_error, caudal_auth::OnError::Deny, "fails closed by default");
        assert!(r.publish && r.play);
        assert_eq!((r.cache, r.deny_cache), (Duration::from_secs(30), Duration::from_secs(5)));
        assert!(Config::default().auth.rewrite_config().unwrap().is_none());
        let bad = |extra: &str| {
            toml::from_str::<Config>(&format!("[auth.rewrite]\nurl = \"https://a/r\"\nsecret = \"{secret}\"\n{extra}"))
                .unwrap()
                .validate()
                .unwrap_err()
        };
        assert!(bad("timeout_ms = 50\n").contains("[auth.rewrite] timeout_ms"));
        assert!(bad("on_error = \"maybe\"\n").contains("on_error"));
        assert!(bad("publish = false\nplay = false\n").contains("both false"));
        assert!(bad("cache_secs = 86400\n").contains("3600"));
        let err = |t: &str| toml::from_str::<Config>(t).unwrap().validate().unwrap_err();
        assert!(err("[auth.rewrite]\nurl = \"route.example\"\nsecret = \"whsec_x\"\n").contains("http(s)"));
        assert!(err("[auth.rewrite]\nurl = \"https://a/r\"\nsecret = \"hunter2\"\n").contains("whsec_"));
    }

    #[test]
    fn auth_source_section() {
        let secret = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";
        let base = format!("[auth.source]\nurl = \"https://src.example/s\"\nsecret = \"{secret}\"\n");
        let ok: Config = toml::from_str(&base).unwrap();
        ok.validate().unwrap();
        let c = ok.auth.source_config().unwrap().unwrap();
        assert_eq!((c.cache, c.deny_cache), (Duration::from_secs(10), Duration::from_secs(5)));
        assert_eq!(ok.auth.source.as_ref().unwrap().idle_secs, 30);
        let bad = |extra: &str| toml::from_str::<Config>(&format!("{base}{extra}")).unwrap().validate().unwrap_err();
        assert!(bad("idle_secs = 0\n").contains("idle_secs"));
        assert!(bad("timeout_ms = 50\n").contains("[auth.source] timeout_ms"));
        assert!(toml::from_str::<Config>(&format!("{base}on_error = \"allow\"\n")).is_err(), "no on_error");
    }

    #[test]
    fn on_demand_pulls() {
        let cfg: Config = toml::from_str(
            "[[http.pull]]\nstream = \"a\"\nurl = \"http://h/a.ts\"\non_demand = true\nidle_secs = 5\n\
             [[http.pull]]\nstream = \"b\"\nurl = \"http://h/b.ts\"\n\
             [[srt.pull]]\nstream = \"c\"\nurl = \"srt://h:9000\"\non_demand = true\n",
        )
        .unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.http.pull_targets().iter().map(|p| p.stream.as_str()).collect::<Vec<_>>(), ["b"]);
        assert!(cfg.srt.pull_targets().is_empty());
        let od = cfg.on_demand_entries();
        assert_eq!(od.len(), 2);
        assert_eq!(od["a"].idle, Duration::from_secs(5));
        assert_eq!(od["c"].idle, crate::on_demand::DEFAULT_IDLE);
        assert_eq!(od["c"].source.kind(), "srt");
        let err = |t: &str| toml::from_str::<Config>(t).unwrap().validate().unwrap_err();
        assert!(
            err("[[http.pull]]\nstream = \"a\"\nurl = \"http://h/a.ts\"\nidle_secs = 5\n").contains("needs `on_demand")
        );
        assert!(
            err("[[rtsp.pull]]\nstream = \"a\"\nurl = \"rtsp://h/a\"\non_demand = true\nidle_secs = 0\n")
                .contains("1..=3600")
        );
        // Still one pull per stream, on demand or not.
        assert!(
            err("[[http.pull]]\nstream = \"a\"\nurl = \"http://h/a.ts\"\non_demand = true\n\
                 [[rtmp.pull]]\nstream = \"a\"\nurl = \"rtmp://h/live/k\"\n")
            .contains("pulled twice")
        );
    }

    #[test]
    fn auto_push_rules() {
        let cfg: Config = toml::from_str(
            "[[push.auto]]\nstreams = [\"cam*\", \"main\"]\nurl = \"srt://203.0.113.4:9000?streamid=$stream\"\n\
             [[push.auto]]\nstreams = [\"*\"]\nurl = \"rtmps://live.example/app/${stream}-key\"\n\
             [[push.auto]]\nstreams = [\"tv\"]\nurl = \"rtp://10.0.0.20:5004\"\nttl = 4\npacing = false\n",
        )
        .unwrap();
        cfg.validate().unwrap();
        let r = &cfg.push.auto;
        assert_eq!(r[0].kind().unwrap(), AutoPushKind::Srt);
        assert!(r[0].matches("cam7") && r[0].matches("main") && !r[0].matches("cam7+720p") && !r[0].matches("x"));
        assert!(r[1].matches("anything") && !r[1].matches("a+b"));
        assert_eq!(expand_push_template(&r[1].url, "show"), "rtmps://live.example/app/show-key");
        assert_eq!(expand_push_template("srt://h:1?streamid=$basename/$stream", "a"), "srt://h:1?streamid=a/a");
        let t = r[2].udp_target("tv").unwrap();
        assert_eq!((t.format, t.ttl, t.pacing), (caudal_multicast::Format::Rtp, 4, false));

        let bad =
            |rule: &str| toml::from_str::<Config>(&format!("[[push.auto]]\n{rule}")).unwrap().validate().unwrap_err();
        assert!(bad("streams = []\nurl = \"srt://h:1\"\n").contains("empty"));
        assert!(bad("streams = [\"c*m\"]\nurl = \"srt://h:1\"\n").contains("pattern"));
        assert!(bad("streams = [\"cam+720p\"]\nurl = \"srt://h:1\"\n").contains("never match"));
        assert!(bad("streams = [\"a\"]\nurl = \"http://h/x\"\n").contains("rtmp://"));
        assert!(bad("streams = [\"a\"]\nurl = \"srt://h\"\n").contains("port"));
        assert!(bad("streams = [\"a\"]\nurl = \"rtmp:///live/$stream\"\n").contains("host"));
        assert!(bad("streams = [\"a\"]\nurl = \"udp://$stream:5000\"\n").contains("no variables"));
        assert!(bad("streams = [\"a\"]\nurl = \"udp://0.0.0.0:5000\"\n").contains("not an address"));
        assert!(bad("streams = [\"a\"]\nurl = \"srt://h:1\"\nttl = 3\n").contains("only apply"));
        // One stream per UDP destination, across static entries and rules.
        let err = toml::from_str::<Config>(
            "[[udp.push]]\nstream = \"tv\"\nurl = \"udp://10.0.0.20:5000\"\n\
             [[push.auto]]\nstreams = [\"*\"]\nurl = \"udp://10.0.0.20:5000\"\n",
        )
        .unwrap()
        .validate()
        .unwrap_err();
        assert!(err.contains("one stream per destination"), "{err}");
    }

    #[test]
    fn debug_output_masks_secrets() {
        let cfg: Config = toml::from_str(
            "[srt]\npassphrase = \"SRTPASSPHRASE\"\n\
             [cluster]\nrole = \"edge\"\nsecret = \"CLUSTERSECRET\"\norigins = [\"http://10.0.0.1:8080\"]\n\
             [auth]\nsecret = \"AUTHSECRET\"\n\
             [auth.hook]\nurl = \"https://gate.example/HOOKPATHSECRET\"\nsecret = \"whsec_GATESECRET\"\n\
             [auth.rewrite]\nurl = \"https://route.example/REWRITEPATHSECRET\"\nsecret = \"whsec_REWRITESECRET\"\n\
             [hooks]\nurls = [\"https://hooks.slack.com/services/SLACKSECRET\"]\nsecret = \"whsec_HOOKSSECRET\"\n\
             [health]\nwebhooks = [\"https://hooks.slack.com/services/HEALTHSECRET\"]\nsecret = \"whsec_HEALTHSECRET\"\n\
             [[push.auto]]\nstreams = [\"cam*\"]\nurl = \"whip://h.example/whip/$stream\"\ntoken = \"AUTOTOKEN\"\n\
             [[admin.users]]\nname = \"ana\"\npassword_hash = \"PASSWORDHASH\"\n",
        )
        .unwrap();
        let shown = format!("{cfg:?}");
        for secret in [
            "SRTPASSPHRASE",
            "CLUSTERSECRET",
            "AUTHSECRET",
            "HOOKPATHSECRET",
            "GATESECRET",
            "REWRITEPATHSECRET",
            "REWRITESECRET",
            "SLACKSECRET",
            "HOOKSSECRET",
            "HEALTHSECRET",
            "AUTOTOKEN",
            "PASSWORDHASH",
        ] {
            assert!(!shown.contains(secret), "{secret} in {shown}");
        }
        assert!(shown.contains("hooks.slack.com") && shown.contains("gate.example"), "hosts stay: {shown}");
    }

    #[test]
    fn whip_pushes() {
        let cfg: Config = toml::from_str(
            "[webrtc]\nudp_bind = \"0.0.0.0:8189\"\n\
             [[webrtc.push]]\nstream = \"main\"\nurl = \"whip://live.example/whip/main\"\ntoken = \"t0k\"\n\
             [[webrtc.push]]\nstream = \"main\"\nurl = \"https://cf.example/0123456789abcdef0123/webRTC/publish\"\n\
             [[push.auto]]\nstreams = [\"cam*\"]\nurl = \"whip+http://10.0.0.7:8080/whip/$stream\"\ntoken = \"x\"\n",
        )
        .unwrap();
        cfg.validate().unwrap();
        let t = cfg.webrtc.push_targets();
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].token.as_deref(), Some("t0k"));
        assert_eq!(cfg.push.auto[0].kind().unwrap(), AutoPushKind::Whip);
        assert!(!format!("{cfg:?}").contains("t0k"), "Debug masks the token");

        // Only the pushes changed: no restart.
        let mut other = cfg.webrtc.clone();
        other.push.clear();
        assert!(!cfg.webrtc.listener_changed(&other));
        other.threads = 2;
        assert!(cfg.webrtc.listener_changed(&other));

        let bad = |t: &str| toml::from_str::<Config>(t).unwrap().validate().unwrap_err();
        assert!(bad("[[webrtc.push]]\nstream = \"a\"\nurl = \"rtmp://h/a/k\"\n").contains("whip://"));
        assert!(bad("[[webrtc.push]]\nstream = \"a b\"\nurl = \"whip://h/w\"\n").contains("stream name"));
        assert!(bad("[[webrtc.push]]\nstream = \"a\"\nurl = \"whip://h/w\"\ntoken = \"a b\"\n").contains("token"));
        assert!(bad("[[push.auto]]\nstreams = [\"a\"]\nurl = \"srt://h:1\"\ntoken = \"x\"\n").contains("only applies"));
        assert!(bad("[[push.auto]]\nstreams = [\"a\"]\nurl = \"whip://\"\n").contains("host"));
    }

    #[test]
    fn auto_push_schedules_and_conditions() {
        let cfg: Config = toml::from_str(
            "[[push.auto]]\nstreams = [\"main\"]\nurl = \"srt://h:1\"\n\
             start = \"2026-12-24T20:00:00-04:00\"\nstop = \"2026-12-24T23:00:00-04:00\"\n\
             [[push.auto]]\nstreams = [\"backup\"]\nurl = \"srt://h:2\"\ninhibit = [\"main\", \"cam*\"]\n\
             enabled = false\ncron = \"0 18 * * MON-FRI\"\ntz = \"America/Puerto_Rico\"\nduration_secs = 3600\n\
             [[push.auto]]\nstreams = [\"x\"]\nurl = \"srt://h:3\"\nstop = \"2026-12-31T00:00:00Z\"\n\
             [[push.auto]]\nstreams = [\"y\"]\nurl = \"srt://h:4\"\nstart = \"2026-12-31T00:00:00Z\"\nduration_secs = 60\n",
        )
        .unwrap();
        cfg.validate().unwrap();
        let r = &cfg.push.auto;
        assert!(r[0].enabled && !r[1].enabled, "enabled defaults to true");
        assert!(r[0].timetable().unwrap().is_some());
        assert_eq!(r[1].inhibited_by(&["cam2".into()]).map(String::as_str), Some("cam2"));
        assert!(r[1].inhibited_by(&["backup".into(), "main+720p".into()]).is_none());

        let bad = |rule: &str| {
            toml::from_str::<Config>(&format!("[[push.auto]]\nstreams = [\"a\"]\nurl = \"srt://h:1\"\n{rule}"))
                .unwrap()
                .validate()
                .unwrap_err()
        };
        assert!(bad("start = \"2026-12-24T20:00:00Z\"\nstop = \"2026-12-24T19:00:00Z\"\n").contains("after `start`"));
        assert!(bad("start = \"2026-12-24T20:00:00Z\"\nstop = \"2026-12-24T20:00:00Z\"\n").contains("after `start`"));
        assert!(bad("start = \"tomorrow\"\n").contains("bad `start`"));
        assert!(bad("stop = \"2026-12-24 20:00\"\n").contains("bad `stop`"));
        assert!(bad("cron = \"every day\"\nduration_secs = 60\n").contains("bad `cron`"));
        assert!(bad("cron = \"0 18 * * *\"\n").contains("needs `duration_secs`"));
        assert!(
            bad("cron = \"0 18 * * *\"\nduration_secs = 60\nstart = \"2026-12-24T20:00:00Z\"\n").contains("not both")
        );
        assert!(bad("cron = \"0 18 * * *\"\nduration_secs = 60\ntz = \"Mars/Base\"\n").contains("bad `tz`"));
        assert!(bad("cron = \"0 18 * * *\"\nduration_secs = 0\n").contains("at least 1"));
        assert!(bad("tz = \"UTC\"\nstart = \"2026-12-24T20:00:00Z\"\n").contains("only applies to `cron`"));
        assert!(bad("duration_secs = 60\n").contains("needs `start` or `cron`"));
        assert!(
            bad("start = \"2026-12-24T20:00:00Z\"\nstop = \"2026-12-24T21:00:00Z\"\nduration_secs = 60\n")
                .contains("not both")
        );
        assert!(bad("inhibit = [\"c*m\"]\n").contains("pattern"));
        // An unknown key is still refused.
        assert!(toml::from_str::<Config>("[[push.auto]]\nstreams = [\"a\"]\nurl = \"srt://h:1\"\nwhen = 1\n").is_err());
    }

    #[test]
    fn udp_push_to_one_receiver() {
        let cfg: Config = toml::from_str(
            "[[udp.push]]\nstream = \"tv\"\nurl = \"udp://10.0.0.20:5000\"\ninterface = \"10.0.0.5\"\n\
             [[udp.push]]\nstream = \"tv\"\nurl = \"rtp://[2001:db8::7]:5004\"\nttl = 8\npacing = false\n\
             [[multicast]]\nstream = \"tv\"\ngroup = \"239.1.1.1:5000\"\n",
        )
        .unwrap();
        cfg.validate().unwrap();
        let t = cfg.multicast_targets().unwrap();
        assert_eq!(t.len(), 3, "one sender for both sections");
        let unicast = t.iter().find(|t| t.group == "10.0.0.20:5000".parse().unwrap()).unwrap();
        assert_eq!(unicast.format, caudal_multicast::Format::Ts);
        assert_eq!(unicast.ttl, 64, "the usual unicast default");
        assert_eq!(unicast.interface, caudal_multicast::Interface::V4("10.0.0.5".parse().unwrap()));
        assert!(unicast.pacing && !unicast.loopback);
        let rtp = t.iter().find(|t| t.group.is_ipv6()).unwrap();
        assert_eq!((rtp.format, rtp.ttl, rtp.pacing), (caudal_multicast::Format::Rtp, 8, false));

        let bad = |t: &str| toml::from_str::<Config>(t).unwrap().validate().unwrap_err();
        assert!(bad("[[udp.push]]\nstream = \"a\"\nurl = \"srt://h:1\"\n").contains("udp://host:port"));
        assert!(bad("[[udp.push]]\nstream = \"a\"\nurl = \"udp://mux.local:5000\"\n").contains("IP address"));
        assert!(bad("[[udp.push]]\nstream = \"a\"\nurl = \"udp://10.0.0.20:0\"\n").contains("port 0"));
        assert!(bad("[[udp.push]]\nstream = \"a\"\nurl = \"udp://0.0.0.0:5000\"\n").contains("not an address"));
        assert!(
            bad("[[udp.push]]\nstream = \"a\"\nurl = \"udp://[2001:db8::7]:5000\"\ninterface = \"10.0.0.5\"\n")
                .contains("IPv6")
        );
        let twice = "[[udp.push]]\nstream = \"a\"\nurl = \"udp://239.1.1.1:5000\"\n\
                     [[multicast]]\nstream = \"b\"\ngroup = \"239.1.1.1:5000\"\n";
        assert!(bad(twice).contains("used twice"));
        // [[multicast]] still wants a group, and says where a unicast goes.
        assert!(bad("[[multicast]]\nstream = \"a\"\ngroup = \"10.0.0.20:5000\"\n").contains("[[udp.push]]"));
    }

    #[test]
    fn pulls_and_pushes_are_validated() {
        let bad = |t: &str| toml::from_str::<Config>(t).unwrap().validate().unwrap_err();
        assert!(bad("[[rtsp.pull]]\nstream = \"a b\"\nurl = \"rtsp://h/x\"\n").contains("not a valid stream name"));
        assert!(bad("[[rtsp.pull]]\nstream = \"a\"\nurl = \"http://h/x\"\n").contains("rtsp://"));
        let twice = "[[rtsp.pull]]\nstream = \"a\"\nurl = \"rtsp://h/1\"\n[[rtsp.pull]]\nstream = \"a\"\nurl = \"rtsp://h/2\"\n";
        assert!(bad(twice).contains("pulled twice"));
        assert!(bad("[[srt.push]]\nstream = \"a\"\nurl = \"udp://h:1\"\n").contains("srt://"));
        assert!(bad("[[restream]]\nstream = \"a\"\nurl = \"srt://h:1\"\n").contains("rtmp://"));
        let ok: Config = toml::from_str(
            "[[srt.push]]\nstream = \"cam+720p\"\nurl = \"srt://h:1\"\n[[restream]]\nstream = \"cam\"\nurl = \"rtmps://h/app/key\"\n",
        )
        .unwrap();
        ok.validate().expect("renditions and rtmps are fine");
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
    fn rtmps_needs_tls_certificates() {
        let parse = |t: &str| toml::from_str::<Config>(t).unwrap();
        assert!(parse("").rtmp.tls_bind.is_none(), "RTMPS is off by default");
        let no_tls = parse("[rtmp]\ntls_bind = \"0.0.0.0:1936\"").validate().unwrap_err();
        assert!(no_tls.contains("[tls]"), "{no_tls}");
        let tls = "[tls]\nbind = \"0.0.0.0:8443\"\ncert = \"c.pem\"\nkey = \"k.pem\"\n";
        let clash = parse(&format!("{tls}[rtmp]\ntls_bind = \"0.0.0.0:8443\"")).validate().unwrap_err();
        assert!(clash.contains("differ"), "{clash}");
        let same = parse(&format!("{tls}[rtmp]\nbind = \"0.0.0.0:1936\"\ntls_bind = \"0.0.0.0:1936\"")).validate();
        assert!(same.unwrap_err().contains("differ"));
        let ok = parse(&format!("{tls}[rtmp]\ntls_bind = \"0.0.0.0:1936\""));
        assert!(ok.validate().is_ok());
        let (bind, source) = ok.rtmp.tls_source(&ok.tls).unwrap().unwrap();
        assert_eq!(bind, "0.0.0.0:1936".parse().unwrap());
        assert!(matches!(source, caudal_tls::CertSource::Files { .. }));
        let acme = parse(
            "[tls]\nbind = \"0.0.0.0:443\"\nacme_domains = [\"live.example\"]\n[rtmp]\ntls_bind = \"0.0.0.0:1936\"",
        );
        assert!(matches!(acme.rtmp.tls_source(&acme.tls).unwrap().unwrap().1, caudal_tls::CertSource::Acme { .. }));
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
