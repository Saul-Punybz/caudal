//! The server's own HTTP surface: health, readiness, the stream inventory
//! and Prometheus metrics. LL-HLS routes are mounted separately by
//! `caudal_hls::router` (agent C).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use caudal_core::{Registry, Stream};
use serde::Serialize;

use crate::metrics;

/// Shared state for the server's own routes.
pub struct AppState {
    pub registry: Arc<Registry>,
    /// Flips to `true` once the RTMP and HTTP listeners are both up.
    pub ready: AtomicBool,
}

impl AppState {
    pub fn new(registry: Arc<Registry>) -> Arc<Self> {
        Arc::new(Self { registry, ready: AtomicBool::new(false) })
    }

    pub fn mark_ready(&self) {
        self.ready.store(true, Ordering::Release);
    }

    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
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
}

impl StreamJson {
    fn from_stream(s: &Stream) -> Self {
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
    Json(state.registry.list().iter().map(|s| StreamJson::from_stream(s)).collect())
}

async fn get_stream(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    match state.registry.get(&name) {
        Some(s) => Json(StreamJson::from_stream(&s)).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn metrics_endpoint(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let body = metrics::render(&state.registry);
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], body)
}

/// The server's own routes: health, readiness, stream inventory, metrics.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/api/v1/streams", get(list_streams))
        .route("/api/v1/streams/{name}", get(get_stream))
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
