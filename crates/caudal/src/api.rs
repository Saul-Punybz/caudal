//! The server's own HTTP surface: health, readiness, the stream inventory
//! and Prometheus metrics. LL-HLS routes are mounted separately by
//! `caudal_hls::router` (agent C).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use caudal_core::{Cue, CueKind, Registry, Stream};
use serde::{Deserialize, Serialize};

use crate::metrics;

/// Shared state for the server's own routes.
pub struct AppState {
    pub registry: Arc<Registry>,
    /// Flips to `true` once the RTMP and HTTP listeners are both up.
    pub ready: AtomicBool,
    /// splice/segmentation event ids for cues inserted over the API.
    cue_seq: AtomicU32,
    /// Set once, if `[health]` starts (see `crate::main::run`); read by
    /// `/metrics` for `caudal_alerts_*`. `GET /api/v1/alerts` itself is
    /// mounted separately by `caudal_health::HealthService::router`.
    health: std::sync::OnceLock<Arc<caudal_health::HealthService>>,
    /// Set once at startup (see `crate::main::run`); read by `/metrics` for
    /// `caudal_access_denied_total`. Always present: `subsystems::Supervisor`
    /// creates a `caudal_access::Checker` even with no `[[access.rules]]`.
    access: std::sync::OnceLock<Arc<caudal_access::Checker>>,
    /// Set once, if `[captions]` captions any stream; read by `/metrics`
    /// for `caudal_captions_*`.
    #[cfg(feature = "captions")]
    captions: std::sync::OnceLock<caudal_captions::Captions>,
    /// Set once on a cluster edge; `/metrics` appends its pull metrics.
    edge: std::sync::OnceLock<caudal_cluster::Edge>,
    /// Set once at startup; `/metrics` appends `caudal_multicast_*`.
    multicast: std::sync::OnceLock<caudal_multicast::MulticastHandle>,
    /// Set once at startup: `/api/v1/omt/sources`, the `omt` fields of the
    /// stream inventory, `caudal_omt_*`.
    omt: std::sync::OnceLock<Arc<crate::omt::OmtRuntime>>,
}

impl AppState {
    pub fn new(registry: Arc<Registry>) -> Arc<Self> {
        Arc::new(Self {
            registry,
            ready: AtomicBool::new(false),
            cue_seq: AtomicU32::new(1),
            health: std::sync::OnceLock::new(),
            access: std::sync::OnceLock::new(),
            #[cfg(feature = "captions")]
            captions: std::sync::OnceLock::new(),
            edge: std::sync::OnceLock::new(),
            multicast: std::sync::OnceLock::new(),
            omt: std::sync::OnceLock::new(),
        })
    }

    pub fn mark_ready(&self) {
        self.ready.store(true, Ordering::Release);
    }

    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    pub fn set_health(&self, service: Arc<caudal_health::HealthService>) {
        let _ = self.health.set(service);
    }

    pub fn set_access(&self, checker: Arc<caudal_access::Checker>) {
        let _ = self.access.set(checker);
    }

    #[cfg(feature = "captions")]
    pub fn set_captions(&self, captions: caudal_captions::Captions) {
        let _ = self.captions.set(captions);
    }

    pub fn set_multicast(&self, handle: caudal_multicast::MulticastHandle) {
        let _ = self.multicast.set(handle);
    }

    pub fn set_omt(&self, omt: Arc<crate::omt::OmtRuntime>) {
        let _ = self.omt.set(omt);
    }

    pub fn set_edge(&self, edge: caudal_cluster::Edge) {
        let _ = self.edge.set(edge);
    }
}

#[derive(Debug, Serialize)]
struct TrackJson {
    id: u32,
    kind: &'static str,
    codec: &'static str,
    timescale: u32,
    width: Option<u32>,
    height: Option<u32>,
    fps: Option<f64>,
    sample_rate: Option<u32>,
    channels: Option<u8>,
    lang: Option<String>,
}

#[derive(Debug, Serialize)]
struct StatsJson {
    frames_in: u64,
    bytes_in: u64,
    frames_buffered: usize,
    bytes_buffered: usize,
    buffered_ms: i64,
    viewers: usize,
}

#[derive(Debug, Serialize)]
struct StreamJson {
    name: String,
    tracks: Vec<TrackJson>,
    stats: StatsJson,
    /// Only on a stream an `[[omt.pull]]` publishes or an `[[omt.output]]`
    /// sends.
    #[serde(skip_serializing_if = "Option::is_none")]
    omt: Option<OmtStreamJson>,
}

#[derive(Debug, Serialize)]
struct OmtStreamJson {
    /// The `[[omt.pull]]` publishing this stream.
    pull: Option<OmtPullJson>,
    /// Every `[[omt.output]]` sending it.
    outputs: Vec<OmtOutputJson>,
}

#[derive(Debug, Serialize)]
struct OmtPullJson {
    source: String,
    /// Every connection to the source is up.
    connected: bool,
    video_in: u64,
    audio_in: u64,
    bytes_in: u64,
    reconnects: u64,
    /// Times the stream was (re)published.
    publishes: u64,
    /// Frames dropped, by reason.
    dropped: std::collections::BTreeMap<&'static str, u64>,
    /// What Caudal tells the source (program while the stream has viewers).
    tally: OmtTallyJson,
}

#[derive(Debug, Serialize)]
struct OmtOutputJson {
    name: String,
    /// `MACHINE (name)`, once announced.
    full_name: Option<String>,
    /// `omt://MACHINE:port`, once listening.
    url: Option<String>,
    receivers: usize,
    frames_sent: u64,
    dropped: std::collections::BTreeMap<&'static str, u64>,
    tally: OmtTallyJson,
}

#[derive(Debug, Serialize)]
struct OmtTallyJson {
    preview: bool,
    program: bool,
}

fn omt_stream_json(omt: &crate::omt::OmtRuntime, stream: &str) -> Option<OmtStreamJson> {
    use std::sync::atomic::Ordering::Relaxed;
    let pull = omt.pulls().into_iter().find(|p| p.stream == stream).map(|p| {
        let st = p.stats.snapshot();
        OmtPullJson {
            source: p.source.clone(),
            connected: st.connected,
            video_in: st.video_in,
            audio_in: st.audio_in,
            bytes_in: st.bytes_in,
            reconnects: st.reconnects,
            publishes: st.publishes,
            dropped: caudal_omt::DropReason::ALL.iter().map(|r| r.as_str()).zip(st.dropped).collect(),
            tally: OmtTallyJson { preview: st.tally.preview, program: st.tally.program },
        }
    });
    let outputs: Vec<OmtOutputJson> = omt
        .outputs()
        .into_iter()
        .filter(|o| o.stream == stream)
        .map(|o| OmtOutputJson {
            name: o.name.clone(),
            full_name: o.full_name.clone(),
            url: o.url.clone(),
            receivers: o.stats.receivers.load(Relaxed),
            frames_sent: o.stats.frames_sent.load(Relaxed),
            dropped: o.stats.dropped().into_iter().collect(),
            tally: OmtTallyJson { preview: o.stats.preview.load(Relaxed), program: o.stats.program.load(Relaxed) },
        })
        .collect();
    (pull.is_some() || !outputs.is_empty()).then_some(OmtStreamJson { pull, outputs })
}

impl StreamJson {
    fn from_stream(s: &Stream, omt: Option<&crate::omt::OmtRuntime>) -> Self {
        let stats = s.stats();
        StreamJson {
            name: s.name().to_string(),
            tracks: s
                .tracks()
                .into_iter()
                .map(|t| TrackJson {
                    id: t.id.0,
                    kind: t.kind().as_str(),
                    codec: t.codec.as_str(),
                    timescale: t.timescale,
                    width: t.video.map(|v| v.width),
                    height: t.video.map(|v| v.height),
                    fps: t.video.and_then(|v| v.fps),
                    sample_rate: t.audio.map(|a| a.sample_rate),
                    channels: t.audio.map(|a| a.channels),
                    lang: t.lang.clone(),
                })
                .collect(),
            stats: StatsJson {
                frames_in: stats.frames_in,
                bytes_in: stats.bytes_in,
                frames_buffered: stats.frames_buffered,
                bytes_buffered: stats.bytes_buffered,
                buffered_ms: stats.buffered_micros / 1000,
                viewers: stats.viewers,
            },
            omt: omt.and_then(|o| omt_stream_json(o, s.name())),
        }
    }
}

async fn healthz() -> &'static str {
    "ok"
}

async fn readyz(State(state): State<Arc<AppState>>) -> StatusCode {
    if state.is_ready() { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE }
}

async fn list_streams(State(state): State<Arc<AppState>>) -> Json<Vec<StreamJson>> {
    let omt = state.omt.get().map(|o| o.as_ref());
    Json(state.registry.list().iter().map(|s| StreamJson::from_stream(s, omt)).collect())
}

async fn get_stream(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    match state.registry.get(&name) {
        Some(s) => Json(StreamJson::from_stream(&s, state.omt.get().map(|o| o.as_ref()))).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Debug, Serialize)]
struct OmtSourceJson {
    /// `MACHINE (Name)`: what `[[omt.pull]] source` takes.
    name: String,
    host: String,
    port: u16,
    /// Best first.
    addresses: Vec<std::net::IpAddr>,
    /// `omt://address:port` with the best address: what `[[omt.pull]] url`
    /// takes.
    url: String,
}

/// How long the first `GET /api/v1/omt/sources` (the one that starts
/// discovery) waits for answers before listing.
const OMT_FIRST_BROWSE: std::time::Duration = std::time::Duration::from_millis(1500);

/// `GET /api/v1/omt/sources`: the OMT sources discovered on the network now
/// (mDNS and/or the discovery server), sorted by name. The first call
/// starts discovery if no pull or output has, and waits 1.5 s for answers.
async fn omt_sources(State(state): State<Arc<AppState>>) -> Response {
    let Some(omt) = state.omt.get().cloned() else {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({ "error": "OMT is not running" })))
            .into_response();
    };
    let listed = tokio::task::spawn_blocking(move || {
        let (sources, fresh) = omt.sources()?;
        if !fresh {
            return Ok(sources);
        }
        std::thread::sleep(OMT_FIRST_BROWSE);
        omt.sources().map(|(s, _)| s)
    })
    .await;
    match listed {
        Ok(Ok(sources)) => Json(
            sources
                .into_iter()
                .map(|s| OmtSourceJson {
                    url: match s.addresses.first() {
                        Some(std::net::IpAddr::V6(a)) => format!("omt://[{a}]:{}", s.port),
                        Some(a) => format!("omt://{a}:{}", s.port),
                        None => format!("omt://{}:{}", s.host.trim_end_matches('.'), s.port),
                    },
                    name: s.full_name,
                    host: s.host,
                    port: s.port,
                    addresses: s.addresses,
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Ok(Err(e)) => (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({ "error": e }))).into_response(),
        Err(e) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response()
        }
    }
}

/// Body of `POST /api/v1/streams/{name}/cues`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CueRequest {
    /// `"out"` or `"in"`. Optional when `section_hex` is given (the
    /// section says what it is); when both are given they must agree.
    kind: Option<String>,
    /// Planned break length, for `"out"` without a section.
    duration_ms: Option<i64>,
    /// A whole `splice_info_section`, hex, sent on as is.
    section_hex: Option<String>,
}

#[derive(Debug, Serialize)]
struct CueJson {
    kind: &'static str,
    /// Media time the cue was placed at: the stream's newest frame.
    at_us: i64,
    duration_ms: Option<i64>,
    section_hex: String,
}

fn bad_request(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": msg }))).into_response()
}

/// Inserts an SCTE-35 cue at the stream's live edge. Without a section,
/// one is built (`time_signal` + `segmentation_descriptor`) whose splice
/// time is that same media time.
async fn post_cue(State(state): State<Arc<AppState>>, Path(name): Path<String>, body: Bytes) -> Response {
    let Some(stream) = state.registry.get(&name) else { return StatusCode::NOT_FOUND.into_response() };
    let req: CueRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return bad_request(&format!("invalid body: {e}")),
    };
    let at_us = stream.newest_micros().unwrap_or(0);
    let (section, kind) = match (&req.section_hex, req.kind.as_deref()) {
        (Some(hex), want) => {
            let Ok(bytes) = caudal_scte35::from_hex(hex) else { return bad_request("section_hex is not hex") };
            let splice = match caudal_scte35::parse(&bytes) {
                Ok(s) => s,
                Err(e) => return bad_request(&e.to_string()),
            };
            if want.is_some_and(|w| w != splice.kind.as_str()) {
                return bad_request("kind does not match the section");
            }
            if req.duration_ms.is_some() {
                return bad_request("duration_ms comes from the section when section_hex is given");
            }
            (Bytes::from(bytes), splice.kind)
        }
        (None, Some(k)) => {
            let kind = match (k, req.duration_ms) {
                ("out", Some(ms)) if ms <= 0 => return bad_request("duration_ms must be positive"),
                ("out", ms) => CueKind::Out { duration_us: ms.map(|ms| ms.saturating_mul(1000)) },
                ("in", None) => CueKind::In,
                ("in", Some(_)) => return bad_request("duration_ms only applies to \"out\""),
                _ => return bad_request("kind must be \"out\" or \"in\""),
            };
            let pts = (caudal_scte35::us_to_ticks(at_us) as u64) & caudal_scte35::PTS_MASK;
            let id = state.cue_seq.fetch_add(1, Ordering::Relaxed);
            match caudal_scte35::build(kind, Some(pts), id, caudal_scte35::Command::TimeSignal) {
                Ok(s) => (s, kind),
                Err(e) => return bad_request(&e.to_string()),
            }
        }
        (None, None) => return bad_request("kind or section_hex is required"),
    };
    let json = CueJson {
        kind: kind.as_str(),
        at_us,
        duration_ms: match kind {
            CueKind::Out { duration_us } => duration_us.map(|us| us / 1000),
            _ => None,
        },
        section_hex: caudal_scte35::to_hex(&section),
    };
    if stream.inject_cue(Cue { at_us, section, kind }).is_err() {
        // Ended between the lookup and the insert.
        return StatusCode::NOT_FOUND.into_response();
    }
    tracing::info!(stream = %name, kind = json.kind, at_us, "scte-35 cue inserted over the API");
    (StatusCode::ACCEPTED, Json(json)).into_response()
}

async fn metrics_endpoint(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut body = metrics::render(
        &state.registry,
        state.health.get().map(|h| h.as_ref()),
        state.access.get().map(|a| a.as_ref()),
    );
    #[cfg(feature = "captions")]
    if let Some(c) = state.captions.get() {
        body.push_str(&crate::captions::render_metrics(c));
    }
    if let Some(edge) = state.edge.get() {
        edge.render_metrics(&mut body);
    }
    if let Some(multicast) = state.multicast.get() {
        multicast.render_metrics(&mut body);
    }
    if let Some(omt) = state.omt.get() {
        metrics::render_omt(&mut body, &omt.pulls(), &omt.outputs());
    }
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], body)
}

/// The server's own routes: health, readiness, stream inventory, metrics.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/api/v1/streams", get(list_streams))
        .route("/api/v1/streams/{name}", get(get_stream))
        .route("/api/v1/streams/{name}/cues", post(post_cue))
        .route("/api/v1/omt/sources", get(omt_sources))
        .route("/metrics", get(metrics_endpoint))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;

    fn app() -> (Router, Arc<AppState>) {
        let state = AppState::new(Registry::new());
        (router(state.clone()), state)
    }

    #[tokio::test]
    async fn healthz_is_always_ok() {
        let (app, _state) = app();
        let res = app.oneshot(Request::builder().uri("/healthz").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn readyz_gates_on_the_flag() {
        let (app, state) = app();
        let res = app.clone().oneshot(Request::builder().uri("/readyz").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        state.mark_ready();
        let res = app.oneshot(Request::builder().uri("/readyz").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn empty_registry_lists_no_streams() {
        let (app, _state) = app();
        let res = app.oneshot(Request::builder().uri("/api/v1/streams").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        assert_eq!(bytes, "[]".as_bytes());
    }

    #[tokio::test]
    async fn unknown_stream_is_404() {
        let (app, _state) = app();
        let res =
            app.oneshot(Request::builder().uri("/api/v1/streams/nope").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn one_stream_serializes_to_the_fixed_shape() {
        use caudal_core::{AudioParams, BufferConfig, Codec, TrackId, TrackInfo, VideoParams};

        let (app, state) = app();
        let publisher = state.registry.publish("test", BufferConfig::default()).unwrap();
        publisher
            .set_tracks(vec![TrackInfo {
                id: TrackId(0),
                codec: Codec::H264,
                timescale: 90_000,
                init: Default::default(),
                lang: None,
                video: Some(VideoParams { width: 1280, height: 720, fps: Some(30.0) }),
                audio: None,
            }])
            .unwrap();

        let res =
            app.oneshot(Request::builder().uri("/api/v1/streams/test").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        let expected = "{\"name\":\"test\",\"tracks\":[{\"id\":0,\"kind\":\"video\",\"codec\":\"h264\",\"timescale\":90000,\"width\":1280,\"height\":720,\"fps\":30.0,\"sample_rate\":null,\"channels\":null,\"lang\":null}],\"stats\":{\"frames_in\":0,\"bytes_in\":0,\"frames_buffered\":0,\"bytes_buffered\":0,\"buffered_ms\":0,\"viewers\":0}}";
        assert_eq!(body, expected);

        // Silence unused-import/variable warnings for AudioParams in case a
        // future edit stops constructing an audio track here.
        let _ = std::mem::size_of::<AudioParams>();
    }

    async fn post_cue_req(app: &Router, stream: &str, body: &str) -> (StatusCode, serde_json::Value) {
        let req = Request::post(format!("/api/v1/streams/{stream}/cues"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    #[tokio::test]
    async fn cues_are_inserted_at_the_live_edge() {
        use caudal_core::{BufferConfig, Codec, Event, Frame, StartAt, TrackId, TrackInfo, VideoParams};

        let (app, state) = app();
        let publisher = state.registry.publish("live", BufferConfig::default()).unwrap();
        publisher
            .set_tracks(vec![TrackInfo {
                id: TrackId(0),
                codec: Codec::H264,
                timescale: 90_000,
                init: Default::default(),
                lang: None,
                video: Some(VideoParams { width: 16, height: 16, fps: None }),
                audio: None,
            }])
            .unwrap();
        let mut sub = publisher.stream().subscribe(StartAt::LiveEdge);
        for n in 0..3 {
            let f = Frame { track: TrackId(0), dts: n * 3000, pts: n * 3000, keyframe: n == 0, data: Bytes::new() };
            publisher.push(f).unwrap();
        }

        let (status, json) = post_cue_req(&app, "live", r#"{"kind":"out","duration_ms":30000}"#).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{json}");
        assert_eq!(json["kind"], "out");
        assert_eq!(json["duration_ms"], 30000);
        assert_eq!(json["at_us"], 66_666, "the newest frame (dts 6000)");
        let hex = json["section_hex"].as_str().unwrap();
        let parsed = caudal_scte35::parse(&caudal_scte35::from_hex(hex).unwrap()).unwrap();
        assert_eq!(parsed.kind, CueKind::Out { duration_us: Some(30_000_000) });
        assert_eq!(parsed.pts_90k, Some(6000));

        // The cue reaches viewers, after the frames already pushed.
        let mut got = None;
        while let Some(ev) = sub.try_recv() {
            if let Event::Cue(c) = ev {
                got = Some(c);
            }
        }
        let cue = got.expect("viewer saw the cue");
        assert_eq!(cue.at_us, 66_666);
        assert_eq!(caudal_scte35::to_hex(&cue.section), hex);

        // A caller-supplied section is passed through unchanged.
        let (status, json) = post_cue_req(&app, "live", &format!(r#"{{"section_hex":"{hex}"}}"#)).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{json}");
        assert_eq!(json["section_hex"], hex);
        assert_eq!(json["kind"], "out");
        let (status, json) = post_cue_req(&app, "live", r#"{"kind":"in"}"#).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{json}");
        assert_eq!(json["duration_ms"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn bad_cue_requests() {
        use caudal_core::BufferConfig;
        let (app, state) = app();
        assert_eq!(post_cue_req(&app, "nope", r#"{"kind":"out"}"#).await.0, StatusCode::NOT_FOUND);
        let _p = state.registry.publish("live", BufferConfig::default()).unwrap();
        for body in [
            "",
            "not json",
            "{}",
            r#"{"kind":"sideways"}"#,
            r#"{"kind":"out","duration_ms":-5}"#,
            r#"{"kind":"in","duration_ms":5}"#,
            r#"{"kind":"out","extra":1}"#,
            r#"{"section_hex":"0xZZ"}"#,
            r#"{"section_hex":"0xFC3000"}"#,
        ] {
            let (status, json) = post_cue_req(&app, "live", body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body} -> {json}");
            assert!(json["error"].is_string(), "{body}");
        }
        // kind contradicting the section.
        let out = caudal_scte35::build(CueKind::Out { duration_us: None }, None, 1, Default::default()).unwrap();
        let body = format!(r#"{{"kind":"in","section_hex":"{}"}}"#, caudal_scte35::to_hex(&out));
        assert_eq!(post_cue_req(&app, "live", &body).await.0, StatusCode::BAD_REQUEST);
    }

    fn omt_app() -> (Router, Arc<AppState>, Arc<crate::omt::OmtRuntime>) {
        use open_media_transport::discovery::{Source, SourceEvent};
        let state = AppState::new(Registry::new());
        let directory = open_media_transport::address::Directory::manual();
        directory.apply(SourceEvent::Resolved(Source {
            full_name: "STUDIO (Camera 2)".into(),
            host: "studio.local.".into(),
            port: 6401,
            addresses: vec!["192.168.1.20".parse().unwrap(), "fe80::1".parse().unwrap()],
        }));
        directory.apply(SourceEvent::Resolved(Source {
            full_name: "BOOTH (Program)".into(),
            host: "booth.local.".into(),
            port: 6400,
            addresses: vec!["fd00::5".parse().unwrap()],
        }));
        let omt = crate::omt::OmtRuntime::with_directory(directory, state.registry.clone());
        state.set_omt(omt.clone());
        (router(state.clone()), state, omt)
    }

    async fn get_json(app: &Router, uri: &str) -> (StatusCode, serde_json::Value) {
        let res = app.clone().oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap()).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    #[tokio::test]
    async fn omt_sources_lists_the_directory() {
        let (app, _state, _omt) = omt_app();
        let (status, json) = get_json(&app, "/api/v1/omt/sources").await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(
            json,
            serde_json::json!([
                {"name": "BOOTH (Program)", "host": "booth.local.", "port": 6400,
                 "addresses": ["fd00::5"], "url": "omt://[fd00::5]:6400"},
                {"name": "STUDIO (Camera 2)", "host": "studio.local.", "port": 6401,
                 "addresses": ["192.168.1.20", "fe80::1"], "url": "omt://192.168.1.20:6401"},
            ])
        );
    }

    #[tokio::test]
    async fn omt_sources_without_omt_is_503() {
        let (app, _state) = app();
        let (status, json) = get_json(&app, "/api/v1/omt/sources").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(json["error"].is_string());
    }

    #[tokio::test]
    async fn streams_carry_omt_fields_only_when_omt_touches_them() {
        use std::sync::atomic::Ordering::Relaxed;

        use caudal_core::BufferConfig;
        use caudal_omt::Quality;

        let (app, state, omt) = omt_app();
        let _cam = state.registry.publish("cam2", BufferConfig::default()).unwrap();
        let _other = state.registry.publish("other", BufferConfig::default()).unwrap();
        omt.reload_for_test(
            // A real pull at a closed loopback port: it never connects.
            vec![caudal_omt::PullConfig {
                stream: "cam2".into(),
                source: "omt://127.0.0.1:1".into(),
                quality: Quality::High,
                video_kbps: 6000,
                audio_kbps: 128,
                ffmpeg: "ffmpeg".into(),
                directory: None,
            }],
            vec![
                caudal_omt::OutputConfig {
                    stream: "cam2".into(),
                    name: "Cam 2 relay".into(),
                    quality: Quality::Default,
                    encoder_threads: 0,
                    discovery: None,
                },
                caudal_omt::OutputConfig {
                    stream: "nowhere".into(),
                    name: "Unrelated".into(),
                    quality: Quality::Default,
                    encoder_threads: 0,
                    discovery: None,
                },
            ],
        );
        let pull = &omt.pulls()[0];
        pull.stats.video_in.store(120, Relaxed);
        pull.stats.dropped[caudal_omt::DropReason::ALL.iter().position(|r| r.as_str() == "decode").unwrap()]
            .store(1, Relaxed);
        let out = &omt.outputs()[0];
        out.stats.receivers.store(1, Relaxed);
        out.stats.preview.store(true, Relaxed);

        let (status, json) = get_json(&app, "/api/v1/streams/cam2").await;
        assert_eq!(status, StatusCode::OK);
        let o = &json["omt"];
        assert_eq!(o["pull"]["source"], "omt://127.0.0.1:1");
        assert_eq!(o["pull"]["connected"], false);
        assert_eq!(o["pull"]["video_in"], 120);
        assert_eq!(o["pull"]["dropped"]["decode"], 1);
        assert_eq!(o["pull"]["dropped"]["queue_full"], 0);
        assert_eq!(o["pull"]["tally"], serde_json::json!({"preview": false, "program": false}));
        assert_eq!(o["outputs"].as_array().unwrap().len(), 1, "only this stream's outputs: {o}");
        assert_eq!(o["outputs"][0]["name"], "Cam 2 relay");
        assert_eq!(o["outputs"][0]["receivers"], 1);
        assert_eq!(o["outputs"][0]["tally"], serde_json::json!({"preview": true, "program": false}));

        let (_, json) = get_json(&app, "/api/v1/streams/other").await;
        assert!(json.get("omt").is_none(), "no omt key on a non-OMT stream: {json}");
        let (_, list) = get_json(&app, "/api/v1/streams").await;
        let list = list.as_array().unwrap();
        assert_eq!(list.iter().filter(|s| s.get("omt").is_some()).count(), 1);

        let res = app.oneshot(Request::builder().uri("/metrics").body(Body::empty()).unwrap()).await.unwrap();
        let body =
            String::from_utf8(axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap();
        assert!(body.contains("caudal_omt_frames_in_total{stream=\"cam2\",kind=\"video\"} 120"), "{body}");
        assert!(body.contains("caudal_omt_receivers{stream=\"cam2\",output=\"Cam 2 relay\"} 1"), "{body}");
    }

    #[tokio::test]
    async fn metrics_reports_prometheus_text() {
        let (app, _state) = app();
        let res = app.oneshot(Request::builder().uri("/metrics").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body.contains("caudal_streams"));
    }
}
