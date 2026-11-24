//! Weighted load balancing across the cluster, after MistServer's
//! `MistUtilLoad` (`src/utils/util_load.cpp`).
//!
//! Every node with `[cluster.balance]` sends a [`LoadReport`] to its
//! origins (an origin also records its own); the origin's [`Balancer`]
//! scores each node for a viewer of a stream and answers with the best one.
//!
//! The score is MistServer's, term for term (`util_load.cpp:247-257`):
//!
//! ```text
//! cpu   = w_cpu * (1 - cpu%)
//! ram   = w_ram * (1 - ram%)
//! bw    = w_bw  * (1 - (bw_out + pending) / capacity)
//! geo   = w_geo * (1 - great_circle_angle / pi)     (both sides have coordinates)
//! bonus = w_bonus                                   (the node already has the stream)
//! score = cpu + ram + bw + geo + bonus
//! ```
//!
//! with MistServer's default weights (cpu 500, ram 500, bw 1000, geo 1000,
//! bonus 50). `pending` is MistServer's `addBandwidth`: each viewer sent to
//! a node adds a guess of one viewer's bandwidth (the node's average,
//! clamped to 0.5-8 Mbit/s, 1 Mbit/s when it has no viewers), and every
//! report from that node multiplies it by 0.75, so a burst of viewers
//! spreads before the next report arrives.
//!
//! Differences from MistServer, on purpose:
//! - A node is excluded (never chosen) when its report is stale, when its
//!   bandwidth plus pending reaches its capacity (MistServer does this
//!   too), when CPU or RAM is over the configured hard limit, or when it
//!   has no public URL or refuses viewers.
//! - When coordinates are missing on either side, two country codes that
//!   match give the full geo weight and two that differ give none
//!   (MistServer has coordinates only).
//! - Ties go to the node that has the stream, then the one with fewer
//!   viewers, then the lower node id (MistServer keeps the first host).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use caudal_core::Cidr;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::token::Secret;

/// Largest report body accepted.
const MAX_REPORT: usize = 256 * 1024;
/// MistServer's per-viewer bandwidth guess bounds and default, in bit/s
/// (`util_load.cpp:327-339`: 64 KiB/s, 1 MiB/s and 128 KiB/s).
const GUESS_MIN_BPS: f64 = 64.0 * 1024.0 * 8.0;
const GUESS_MAX_BPS: f64 = 1024.0 * 1024.0 * 8.0;
const GUESS_DEFAULT_BPS: f64 = 128.0 * 1024.0 * 8.0;
/// MistServer's `addBandwidth *= 0.75` on every update.
const PENDING_DECAY: f64 = 0.75;

/// A point on the globe, degrees.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GeoPoint {
    pub lat: f64,
    pub lon: f64,
}

impl GeoPoint {
    /// Great-circle angle to `other` as a fraction of a half turn: 0 for
    /// the same point, 1 for the antipode. MistServer's `geoDist`
    /// (`util_load.cpp:76-80`).
    pub fn distance(&self, other: &GeoPoint) -> f64 {
        let (a, b) = (self.lat.to_radians(), other.lat.to_radians());
        let d = (self.lon - other.lon).to_radians();
        let cos = a.sin() * b.sin() + a.cos() * b.cos() * d.cos();
        cos.clamp(-1.0, 1.0).acos() / std::f64::consts::PI
    }
}

/// What one node says about itself, every report interval.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoadReport {
    pub node_id: String,
    /// Base URL viewers are sent to (`https://edge-1.example.com`). `None`:
    /// reported, never chosen.
    pub url: Option<String>,
    pub accepts_viewers: bool,
    /// Whole-machine CPU use, 0-100.
    pub cpu_percent: f64,
    /// Whole-machine memory use, 0-100.
    pub ram_percent: f64,
    /// Bytes sent to viewers over the last interval, as bit/s.
    pub bw_out_bps: u64,
    /// Configured outbound capacity, bit/s.
    pub bw_capacity_bps: u64,
    /// Open viewer sessions, every protocol.
    pub viewers: u64,
    /// Streams live on the node right now (published or pulled).
    pub streams: Vec<String>,
    pub location: Option<GeoPoint>,
    /// Upper-case ISO 3166-1 alpha-2.
    pub country: Option<String>,
}

/// MistServer's weights (`util_load.cpp:20-24`); the defaults are its.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Weights {
    pub cpu: u64,
    pub ram: u64,
    pub bw: u64,
    pub geo: u64,
    pub bonus: u64,
}

impl Default for Weights {
    fn default() -> Self {
        Self { cpu: 500, ram: 500, bw: 1000, geo: 1000, bonus: 50 }
    }
}

/// The origin's scoring settings.
#[derive(Debug, Clone, PartialEq)]
pub struct BalanceConfig {
    pub weights: Weights,
    /// A node over this CPU use is not chosen.
    pub max_cpu_percent: f64,
    /// A node over this memory use is not chosen.
    pub max_ram_percent: f64,
    /// A node whose last report is older than this is not chosen.
    pub stale_after: Duration,
}

impl Default for BalanceConfig {
    fn default() -> Self {
        Self {
            weights: Weights::default(),
            max_cpu_percent: 95.0,
            max_ram_percent: 95.0,
            stale_after: Duration::from_secs(6),
        }
    }
}

/// Where the viewer is, as far as anyone knows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Viewer {
    pub point: Option<GeoPoint>,
    pub country: Option<String>,
}

/// Why a node cannot take a viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Excluded {
    Stale,
    NoUrl,
    RefusesViewers,
    NoCapacity,
    OverBandwidth,
    OverCpu,
    OverRam,
}

impl Excluded {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stale => "stale",
            Self::NoUrl => "no_url",
            Self::RefusesViewers => "refuses_viewers",
            Self::NoCapacity => "no_capacity",
            Self::OverBandwidth => "over_bandwidth",
            Self::OverCpu => "over_cpu",
            Self::OverRam => "over_ram",
        }
    }
}

/// One node's score, term by term.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct Score {
    pub cpu: u64,
    pub ram: u64,
    pub bw: u64,
    pub geo: u64,
    pub bonus: u64,
    pub total: u64,
}

/// Scores one node for a viewer of `stream`. `pending_bps` is the bandwidth
/// already promised to viewers sent there since its last report.
pub fn score(
    report: &LoadReport,
    age: Duration,
    pending_bps: f64,
    stream: &str,
    viewer: &Viewer,
    cfg: &BalanceConfig,
) -> Result<Score, Excluded> {
    if age > cfg.stale_after {
        return Err(Excluded::Stale);
    }
    if report.url.is_none() {
        return Err(Excluded::NoUrl);
    }
    if !report.accepts_viewers {
        return Err(Excluded::RefusesViewers);
    }
    if report.bw_capacity_bps == 0 {
        return Err(Excluded::NoCapacity);
    }
    let bw_used = (report.bw_out_bps as f64 + pending_bps.max(0.0)) / report.bw_capacity_bps as f64;
    if bw_used >= 1.0 {
        return Err(Excluded::OverBandwidth);
    }
    if report.cpu_percent > cfg.max_cpu_percent {
        return Err(Excluded::OverCpu);
    }
    if report.ram_percent > cfg.max_ram_percent {
        return Err(Excluded::OverRam);
    }
    let w = &cfg.weights;
    let part = |weight: u64, used: f64| (weight as f64 * (1.0 - used.clamp(0.0, 1.0))) as u64;
    let cpu = part(w.cpu, report.cpu_percent / 100.0);
    let ram = part(w.ram, report.ram_percent / 100.0);
    let bw = part(w.bw, bw_used);
    let geo = match (&report.location, &viewer.point) {
        (Some(node), Some(v)) => part(w.geo, node.distance(v)),
        _ => match (&report.country, &viewer.country) {
            (Some(a), Some(b)) if a.eq_ignore_ascii_case(b) => w.geo,
            _ => 0,
        },
    };
    let bonus = if report.streams.iter().any(|s| s == stream) { w.bonus } else { 0 };
    Ok(Score { cpu, ram, bw, geo, bonus, total: cpu + ram + bw + geo + bonus })
}

/// MistServer's guess of what one more viewer costs a node, bit/s.
fn viewer_guess(report: &LoadReport) -> f64 {
    if report.viewers == 0 {
        return GUESS_DEFAULT_BPS;
    }
    (report.bw_out_bps as f64 / report.viewers as f64).clamp(GUESS_MIN_BPS, GUESS_MAX_BPS)
}

struct Node {
    report: LoadReport,
    received: Instant,
    pending_bps: f64,
    picks: u64,
}

/// One node as `GET /api/v1/balance` shows it.
#[derive(Debug, Clone, Serialize)]
pub struct Candidate {
    pub node_id: String,
    pub url: Option<String>,
    pub age_ms: u64,
    pub pending_bps: u64,
    pub picks: u64,
    pub score: Option<Score>,
    pub excluded: Option<Excluded>,
    pub report: LoadReport,
}

/// The node a viewer should go to.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Pick {
    pub node_id: String,
    pub url: String,
    pub score: u64,
}

/// The origin's table of nodes and their last reports.
pub struct Balancer {
    cfg: BalanceConfig,
    nodes: Mutex<HashMap<String, Node>>,
    no_candidate: AtomicU64,
}

impl Balancer {
    pub fn new(cfg: BalanceConfig) -> Arc<Self> {
        Arc::new(Self { cfg, nodes: Mutex::default(), no_candidate: AtomicU64::new(0) })
    }

    pub fn config(&self) -> &BalanceConfig {
        &self.cfg
    }

    /// Records a node's report (its own, or one received from a node).
    pub fn update(&self, report: LoadReport) {
        self.update_at(report, Instant::now());
    }

    fn update_at(&self, report: LoadReport, now: Instant) {
        let mut nodes = self.nodes.lock();
        match nodes.get_mut(&report.node_id) {
            Some(n) => {
                n.report = report;
                n.received = now;
                n.pending_bps *= PENDING_DECAY;
            }
            None => {
                nodes.insert(report.node_id.clone(), Node { report, received: now, pending_bps: 0.0, picks: 0 });
            }
        }
    }

    /// Every known node scored for a viewer of `stream`, best first; the
    /// excluded ones last, by node id.
    pub fn candidates(&self, stream: &str, viewer: &Viewer) -> Vec<Candidate> {
        self.candidates_at(stream, viewer, Instant::now())
    }

    fn candidates_at(&self, stream: &str, viewer: &Viewer, now: Instant) -> Vec<Candidate> {
        let nodes = self.nodes.lock();
        let mut v: Vec<Candidate> = nodes
            .values()
            .map(|n| {
                let age = now.saturating_duration_since(n.received);
                let s = score(&n.report, age, n.pending_bps, stream, viewer, &self.cfg);
                Candidate {
                    node_id: n.report.node_id.clone(),
                    url: n.report.url.clone(),
                    age_ms: age.as_millis() as u64,
                    pending_bps: n.pending_bps as u64,
                    picks: n.picks,
                    score: s.ok(),
                    excluded: s.err(),
                    report: n.report.clone(),
                }
            })
            .collect();
        v.sort_by(|a, b| match (&a.score, &b.score) {
            (Some(x), Some(y)) => y
                .total
                .cmp(&x.total)
                .then(y.bonus.cmp(&x.bonus))
                .then(a.report.viewers.cmp(&b.report.viewers))
                .then(a.node_id.cmp(&b.node_id)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.node_id.cmp(&b.node_id),
        });
        v
    }

    /// The best node for `stream`, without counting a viewer against it.
    pub fn best(&self, stream: &str, viewer: &Viewer) -> Option<Pick> {
        self.candidates(stream, viewer).into_iter().next().and_then(to_pick)
    }

    /// The best node for one new viewer of `stream`: like [`Self::best`],
    /// and adds the viewer's guessed bandwidth to that node's pending load.
    pub fn pick(&self, stream: &str, viewer: &Viewer) -> Option<Pick> {
        self.pick_at(stream, viewer, Instant::now())
    }

    fn pick_at(&self, stream: &str, viewer: &Viewer, now: Instant) -> Option<Pick> {
        let Some(p) = self.candidates_at(stream, viewer, now).into_iter().next().and_then(to_pick) else {
            self.no_candidate.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        if let Some(n) = self.nodes.lock().get_mut(&p.node_id) {
            n.pending_bps += viewer_guess(&n.report);
            n.picks += 1;
        }
        Some(p)
    }

    /// Appends `caudal_cluster_balance_*` to `out`.
    pub fn render_metrics(&self, out: &mut String) {
        let now = Instant::now();
        let mut rows: Vec<(String, Option<Excluded>, u64, LoadReport, f64)> = self
            .nodes
            .lock()
            .values()
            .map(|n| {
                let age = now.saturating_duration_since(n.received);
                // Stream-independent: no bonus, no viewer location.
                let s = score(&n.report, age, n.pending_bps, "", &Viewer::default(), &self.cfg);
                (n.report.node_id.clone(), s.err(), n.picks, n.report.clone(), age.as_secs_f64())
            })
            .collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        let _ = writeln!(out, "# HELP caudal_cluster_balance_node_up Node can take viewers (1) or is excluded (0).");
        let _ = writeln!(out, "# TYPE caudal_cluster_balance_node_up gauge");
        for (id, ex, ..) in &rows {
            let _ = writeln!(
                out,
                "caudal_cluster_balance_node_up{{node=\"{}\",reason=\"{}\"}} {}",
                crate::esc(id),
                ex.map_or("", Excluded::as_str),
                u8::from(ex.is_none())
            );
        }
        type Gauge = (&'static str, &'static str, fn(&LoadReport) -> f64);
        let gauges: [Gauge; 4] = [
            ("cpu_percent", "Last reported whole-machine CPU use, percent.", |r| r.cpu_percent),
            ("ram_percent", "Last reported whole-machine memory use, percent.", |r| r.ram_percent),
            ("bandwidth_out_bps", "Last reported outbound bandwidth, bit/s.", |r| r.bw_out_bps as f64),
            ("viewers", "Last reported open viewer sessions.", |r| r.viewers as f64),
        ];
        for (name, help, f) in gauges {
            let _ = writeln!(out, "# HELP caudal_cluster_balance_node_{name} {help}");
            let _ = writeln!(out, "# TYPE caudal_cluster_balance_node_{name} gauge");
            for (id, _, _, r, _) in &rows {
                let _ = writeln!(out, "caudal_cluster_balance_node_{name}{{node=\"{}\"}} {}", crate::esc(id), f(r));
            }
        }
        let _ = writeln!(out, "# HELP caudal_cluster_balance_report_age_seconds Time since the node's last report.");
        let _ = writeln!(out, "# TYPE caudal_cluster_balance_report_age_seconds gauge");
        for (id, _, _, _, age) in &rows {
            let _ = writeln!(out, "caudal_cluster_balance_report_age_seconds{{node=\"{}\"}} {age:.3}", crate::esc(id));
        }
        let _ = writeln!(out, "# HELP caudal_cluster_balance_picks_total Viewers sent to the node.");
        let _ = writeln!(out, "# TYPE caudal_cluster_balance_picks_total counter");
        for (id, _, picks, ..) in &rows {
            let _ = writeln!(out, "caudal_cluster_balance_picks_total{{node=\"{}\"}} {picks}", crate::esc(id));
        }
        let _ = writeln!(out, "# HELP caudal_cluster_balance_no_candidate_total Balance requests no node could take.");
        let _ = writeln!(out, "# TYPE caudal_cluster_balance_no_candidate_total counter");
        let _ =
            writeln!(out, "caudal_cluster_balance_no_candidate_total {}", self.no_candidate.load(Ordering::Relaxed));
    }
}

fn to_pick(c: Candidate) -> Option<Pick> {
    Some(Pick { node_id: c.node_id, url: c.url?, score: c.score?.total })
}

/// Playback path on a node for `?proto=` of the redirect endpoint.
fn proto_path(proto: &str, name: &str) -> Option<String> {
    Some(match proto {
        "hls" => format!("/hls/{name}/master.m3u8"),
        "play" | "player" => format!("/play/{name}"),
        "mp4" => format!("/mp4/{name}.mp4"),
        "ts" => format!("/ts/{name}.ts"),
        "mp3" => format!("/audio/{name}.mp3"),
        "aac" => format!("/audio/{name}.aac"),
        "whep" | "webrtc" => format!("/whep/{name}"),
        _ => return None,
    })
}

#[derive(Clone)]
struct RouterState {
    balancer: Arc<Balancer>,
    secret: Secret,
    geo: Option<Arc<caudal_access::GeoIp>>,
    trusted_proxies: Vec<Cidr>,
}

/// The balancer's HTTP side, for an origin:
///
/// - `POST /api/v1/cluster/report` (cluster token): a node's
///   [`LoadReport`]; the token's issuer must be the report's `node_id`.
/// - `GET /cluster/balance/{name}` (public, for players): `200
///   {"stream","node_id","url","score"}` for the best node, `503` when none
///   can take the viewer. The viewer's position comes from `lat`/`lon`
///   query parameters or `X-Latitude`/`X-Longitude` headers (MistServer's
///   names), else from the GeoIP database for the client address.
/// - `GET /cluster/redirect/{name}?proto=hls` (public): `307` to the best
///   node's playback URL for `proto` (`hls`, `play`, `mp4`, `ts`, `mp3`,
///   `aac`, `whep`; default `hls`), keeping the other query parameters
///   (stream tokens). MistServer answers `307` too.
/// - `GET /api/v1/balance?stream=` (behind the admin login): every node,
///   its last report, score terms or why it is excluded.
pub fn router(
    balancer: Arc<Balancer>,
    secret: Secret,
    geo: Option<Arc<caudal_access::GeoIp>>,
    trusted_proxies: Vec<Cidr>,
) -> axum::Router {
    axum::Router::new()
        .route("/api/v1/cluster/report", post(report))
        .route("/cluster/balance/{name}", get(balance))
        .route("/cluster/redirect/{name}", get(redirect))
        .route("/api/v1/balance", get(table))
        .with_state(RouterState { balancer, secret, geo, trusted_proxies })
}

async fn report(State(st): State<RouterState>, req: Request) -> Response {
    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);
    let Some(token) = token else {
        return (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Bearer")], "cluster token required")
            .into_response();
    };
    let Some(node) = st.secret.verify(token) else {
        return (StatusCode::FORBIDDEN, "bad cluster token").into_response();
    };
    let body = match axum::body::to_bytes(req.into_body(), MAX_REPORT).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "report too large").into_response(),
    };
    let r: LoadReport = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("bad report: {e}")).into_response(),
    };
    if r.node_id != node {
        return (StatusCode::BAD_REQUEST, "report node_id is not the token's node").into_response();
    }
    if !r.cpu_percent.is_finite() || !r.ram_percent.is_finite() {
        return (StatusCode::BAD_REQUEST, "bad report: cpu/ram not a number").into_response();
    }
    st.balancer.update(r);
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Deserialize, Default)]
struct GeoQuery {
    lat: Option<f64>,
    lon: Option<f64>,
}

fn viewer(st: &RouterState, q: &GeoQuery, headers: &HeaderMap, peer: Option<SocketAddr>) -> Viewer {
    let h = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<f64>().ok());
    let lat = h("x-latitude").or(q.lat);
    let lon = h("x-longitude").or(q.lon);
    let mut v = Viewer::default();
    if let (Some(lat), Some(lon)) = (lat, lon)
        && lat.is_finite()
        && lon.is_finite()
    {
        v.point = Some(GeoPoint { lat, lon });
    }
    if let (Some(geo), Some(peer)) = (&st.geo, peer) {
        let xff = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok());
        let ip: IpAddr = caudal_core::net::resolve_forwarded(peer.ip(), xff, &st.trusted_proxies);
        v.country = geo.country(ip);
        if v.point.is_none() {
            v.point = geo.location(ip).map(|(lat, lon)| GeoPoint { lat, lon });
        }
    }
    v
}

fn peer(req: &Request) -> Option<SocketAddr> {
    req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0)
}

async fn balance(
    State(st): State<RouterState>,
    Path(name): Path<String>,
    Query(q): Query<GeoQuery>,
    req: Request,
) -> Response {
    let v = viewer(&st, &q, req.headers(), peer(&req));
    match st.balancer.pick(&name, &v) {
        Some(p) => {
            let body = serde_json::json!({ "stream": name, "node_id": p.node_id, "url": p.url, "score": p.score });
            ([(header::CONTENT_TYPE, "application/json")], body.to_string()).into_response()
        }
        None => (StatusCode::SERVICE_UNAVAILABLE, "no node can take a viewer").into_response(),
    }
}

async fn redirect(State(st): State<RouterState>, Path(name): Path<String>, req: Request) -> Response {
    let mut proto = "hls".to_owned();
    let mut geo = GeoQuery::default();
    let mut rest = url::form_urlencoded::Serializer::new(String::new());
    let mut kept = false;
    for (k, v) in url::form_urlencoded::parse(req.uri().query().unwrap_or("").as_bytes()) {
        match &*k {
            "proto" => proto = v.into_owned(),
            "lat" => geo.lat = v.parse().ok(),
            "lon" => geo.lon = v.parse().ok(),
            _ => {
                rest.append_pair(&k, &v);
                kept = true;
            }
        }
    }
    let Some(path) = proto_path(&proto, &name) else {
        return (StatusCode::BAD_REQUEST, "unknown proto (hls, play, mp4, ts, mp3, aac, whep)").into_response();
    };
    let v = viewer(&st, &geo, req.headers(), peer(&req));
    let Some(p) = st.balancer.pick(&name, &v) else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no node can take a viewer").into_response();
    };
    let mut location = format!("{}{path}", p.url.trim_end_matches('/'));
    if kept {
        location.push('?');
        location.push_str(&rest.finish());
    }
    (StatusCode::TEMPORARY_REDIRECT, [(header::LOCATION, location.clone())], location).into_response()
}

#[derive(Deserialize)]
struct TableQuery {
    stream: Option<String>,
}

async fn table(State(st): State<RouterState>, Query(q): Query<TableQuery>) -> Response {
    let stream = q.stream.unwrap_or_default();
    let nodes = st.balancer.candidates(&stream, &Viewer::default());
    let c = st.balancer.config();
    axum::Json(serde_json::json!({
        "stream": stream,
        "weights": c.weights,
        "max_cpu_percent": c.max_cpu_percent,
        "max_ram_percent": c.max_ram_percent,
        "stale_after_secs": c.stale_after.as_secs_f64(),
        "nodes": nodes,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str) -> LoadReport {
        LoadReport {
            node_id: id.into(),
            url: Some(format!("http://{id}")),
            accepts_viewers: true,
            cpu_percent: 0.0,
            ram_percent: 0.0,
            bw_out_bps: 0,
            bw_capacity_bps: 1_000_000_000,
            viewers: 0,
            streams: vec![],
            location: None,
            country: None,
        }
    }

    fn s(r: &LoadReport) -> Result<Score, Excluded> {
        score(r, Duration::ZERO, 0.0, "cam", &Viewer::default(), &BalanceConfig::default())
    }

    #[test]
    fn idle_node_scores_every_weight_but_geo_and_bonus() {
        let sc = s(&node("a")).unwrap();
        assert_eq!(sc, Score { cpu: 500, ram: 500, bw: 1000, geo: 0, bonus: 0, total: 2000 });
    }

    #[test]
    fn terms_follow_mistserver_formula() {
        let mut r = node("a");
        r.cpu_percent = 50.0;
        r.ram_percent = 25.0;
        r.bw_out_bps = 250_000_000;
        r.streams = vec!["cam".into()];
        let sc = s(&r).unwrap();
        assert_eq!((sc.cpu, sc.ram, sc.bw, sc.bonus), (250, 375, 750, 50));
        assert_eq!(sc.total, 250 + 375 + 750 + 50);
    }

    #[test]
    fn weights_change_the_winner() {
        // a: busy CPU, idle link. b: idle CPU, busy link.
        let mut a = node("a");
        a.cpu_percent = 80.0;
        let mut b = node("b");
        b.bw_out_bps = 800_000_000;
        let default = BalanceConfig::default();
        let at = |r: &LoadReport, c: &BalanceConfig| {
            score(r, Duration::ZERO, 0.0, "cam", &Viewer::default(), c).unwrap().total
        };
        // Default weights value bandwidth twice as much as CPU: a wins.
        assert!(at(&a, &default) > at(&b, &default));
        let cpu_heavy = BalanceConfig { weights: Weights { cpu: 3000, ..Weights::default() }, ..default.clone() };
        assert!(at(&b, &cpu_heavy) > at(&a, &cpu_heavy));
    }

    #[test]
    fn hard_limits_exclude() {
        let c = BalanceConfig::default();
        let mut r = node("a");
        r.bw_out_bps = r.bw_capacity_bps;
        assert_eq!(s(&r), Err(Excluded::OverBandwidth));
        // Pending load counts toward the limit, as in MistServer.
        let r = node("a");
        let pend = r.bw_capacity_bps as f64;
        assert_eq!(score(&r, Duration::ZERO, pend, "cam", &Viewer::default(), &c), Err(Excluded::OverBandwidth));
        let mut r = node("a");
        r.cpu_percent = 96.0;
        assert_eq!(s(&r), Err(Excluded::OverCpu));
        let mut r = node("a");
        r.ram_percent = 99.0;
        assert_eq!(s(&r), Err(Excluded::OverRam));
        let mut r = node("a");
        r.url = None;
        assert_eq!(s(&r), Err(Excluded::NoUrl));
        let mut r = node("a");
        r.accepts_viewers = false;
        assert_eq!(s(&r), Err(Excluded::RefusesViewers));
        let mut r = node("a");
        r.bw_capacity_bps = 0;
        assert_eq!(s(&r), Err(Excluded::NoCapacity));
        // Exactly at the CPU limit is still allowed.
        let mut r = node("a");
        r.cpu_percent = 95.0;
        assert!(s(&r).is_ok());
    }

    #[test]
    fn stale_reports_are_excluded() {
        let c = BalanceConfig::default();
        let r = node("a");
        assert!(score(&r, c.stale_after, 0.0, "cam", &Viewer::default(), &c).is_ok());
        let late = c.stale_after + Duration::from_millis(1);
        assert_eq!(score(&r, late, 0.0, "cam", &Viewer::default(), &c), Err(Excluded::Stale));

        let b = Balancer::new(c.clone());
        let t0 = Instant::now();
        b.update_at(node("a"), t0);
        b.update_at(node("b"), t0 + Duration::from_secs(5));
        let now = t0 + c.stale_after + Duration::from_secs(1);
        let cands = b.candidates_at("cam", &Viewer::default(), now);
        assert_eq!(cands[0].node_id, "b");
        assert_eq!(cands[1].excluded, Some(Excluded::Stale));
        assert_eq!(b.pick_at("cam", &Viewer::default(), now).unwrap().node_id, "b");
        let much_later = now + Duration::from_secs(60);
        assert_eq!(b.pick_at("cam", &Viewer::default(), much_later), None);
    }

    #[test]
    fn geo_distance() {
        let sj = GeoPoint { lat: 18.4655, lon: -66.1057 };
        let madrid = GeoPoint { lat: 40.4168, lon: -3.7038 };
        assert!(sj.distance(&sj).abs() < 1e-9);
        // San Juan-Madrid is about 6,300 km; a half turn is ~20,015 km.
        let d = sj.distance(&madrid);
        assert!((d - 6300.0 / 20015.0).abs() < 0.01, "{d}");
        let antipode = GeoPoint { lat: -18.4655, lon: 113.8943 };
        assert!((sj.distance(&antipode) - 1.0).abs() < 1e-6);

        let mut near = node("near");
        near.location = Some(sj);
        let mut far = node("far");
        far.location = Some(madrid);
        let c = BalanceConfig::default();
        let v = Viewer { point: Some(GeoPoint { lat: 18.2, lon: -66.5 }), country: None };
        let g = |r: &LoadReport| score(r, Duration::ZERO, 0.0, "cam", &v, &c).unwrap().geo;
        assert!(g(&near) > 995, "{}", g(&near));
        assert!((660..700).contains(&g(&far)), "{}", g(&far));
        // No viewer position: no geo term at all.
        assert_eq!(s(&near).unwrap().geo, 0);
    }

    #[test]
    fn geo_by_country_when_coordinates_are_missing() {
        let c = BalanceConfig::default();
        let mut pr = node("pr");
        pr.country = Some("PR".into());
        let v = Viewer { point: None, country: Some("pr".into()) };
        assert_eq!(score(&pr, Duration::ZERO, 0.0, "cam", &v, &c).unwrap().geo, 1000);
        let v = Viewer { point: None, country: Some("ES".into()) };
        assert_eq!(score(&pr, Duration::ZERO, 0.0, "cam", &v, &c).unwrap().geo, 0);
    }

    #[test]
    fn ties_prefer_stream_then_fewer_viewers_then_id() {
        let c = BalanceConfig { weights: Weights { bonus: 0, ..Weights::default() }, ..BalanceConfig::default() };
        let b = Balancer::new(c);
        let t = Instant::now();
        let mut x = node("x");
        x.viewers = 5;
        let mut y = node("y");
        y.viewers = 2;
        b.update_at(x, t);
        b.update_at(y, t);
        b.update_at(node("z"), t);
        let order: Vec<String> = b.candidates_at("cam", &Viewer::default(), t).into_iter().map(|c| c.node_id).collect();
        assert_eq!(order, ["z", "y", "x"]);

        // With the default bonus, the node that has the stream wins.
        let b = Balancer::new(BalanceConfig::default());
        let mut has = node("m");
        has.streams = vec!["cam".into()];
        b.update_at(node("a"), t);
        b.update_at(has, t);
        assert_eq!(b.best("cam", &Viewer::default()).unwrap().node_id, "m");
        // Identical nodes: lower id.
        let b = Balancer::new(BalanceConfig::default());
        b.update_at(node("b"), t);
        b.update_at(node("a"), t);
        assert_eq!(b.best("cam", &Viewer::default()).unwrap().node_id, "a");
    }

    #[test]
    fn picks_add_pending_bandwidth_until_the_next_report() {
        let b = Balancer::new(BalanceConfig::default());
        let t = Instant::now();
        let mut a = node("a");
        a.bw_capacity_bps = 10_000_000;
        let mut c = node("c");
        c.bw_capacity_bps = 10_000_000;
        c.cpu_percent = 10.0; // c starts 50 points behind
        b.update_at(a.clone(), t);
        b.update_at(c, t);
        // a takes the first viewer; its 1 Mbit/s guess costs it 100 bw
        // points, so the second goes to c.
        assert_eq!(b.pick_at("cam", &Viewer::default(), t).unwrap().node_id, "a");
        assert_eq!(b.pick_at("cam", &Viewer::default(), t).unwrap().node_id, "c");
        // A new report from a decays its pending load by 0.75.
        let before =
            b.candidates_at("cam", &Viewer::default(), t).iter().find(|c| c.node_id == "a").unwrap().pending_bps;
        b.update_at(a, t);
        let after =
            b.candidates_at("cam", &Viewer::default(), t).iter().find(|c| c.node_id == "a").unwrap().pending_bps;
        assert_eq!(after, (before as f64 * 0.75) as u64);
    }

    #[test]
    fn viewer_guess_is_clamped() {
        let mut r = node("a");
        assert_eq!(viewer_guess(&r), GUESS_DEFAULT_BPS);
        r.viewers = 10;
        r.bw_out_bps = 1_000;
        assert_eq!(viewer_guess(&r), GUESS_MIN_BPS);
        r.bw_out_bps = 1_000_000_000;
        assert_eq!(viewer_guess(&r), GUESS_MAX_BPS);
    }

    #[test]
    fn no_candidate_is_counted() {
        let b = Balancer::new(BalanceConfig::default());
        assert_eq!(b.pick("cam", &Viewer::default()), None);
        let mut out = String::new();
        b.render_metrics(&mut out);
        assert!(out.contains("caudal_cluster_balance_no_candidate_total 1"), "{out}");
    }

    #[test]
    fn proto_paths() {
        assert_eq!(proto_path("hls", "cam").unwrap(), "/hls/cam/master.m3u8");
        assert_eq!(proto_path("whep", "cam").unwrap(), "/whep/cam");
        assert!(proto_path("rtmp", "cam").is_none());
    }
}
