//! `GET /api/v1/streams/{name}/thumbnail.jpg`: a JPEG of a live stream's
//! newest video keyframe, for the dashboard and operators.
//!
//! The keyframe (with its track's parameter sets, which `caudal_ts::TsMux`
//! puts back in front of it) is muxed to MPEG-TS and piped through ffmpeg,
//! which decodes it, scales it and writes one MJPEG picture back. Every
//! video codec `TsMux` carries works: H.264, H.265 and MPEG-2 video.
//!
//! Many dashboards polling one stream cost one ffmpeg: each (stream, width)
//! has one slot, held while its picture is made, so concurrent requests
//! wait for that run instead of starting their own. A picture is reused
//! while it is younger than [`FRESH`], and for as long as the stream's
//! newest keyframe is still the one it shows. At most [`MAX_RUNS`] ffmpeg
//! run at once across all streams.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::Bytes;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use caudal_core::{Codec, Event, Frame, StartAt, Stream, TrackInfo, TrackKind};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Width when the request gives none.
pub const DEFAULT_WIDTH: u32 = 320;
/// Largest `?width=` accepted.
pub const MAX_WIDTH: u32 = 1280;
/// Smallest `?width=` accepted.
pub const MIN_WIDTH: u32 = 16;
/// A picture this young is served again even if a newer keyframe arrived.
pub const FRESH: Duration = Duration::from_secs(3);
/// `Cache-Control: max-age` of a picture.
const MAX_AGE_SECS: u32 = 2;
/// ffmpeg processes running at once, all streams together.
pub const MAX_RUNS: usize = 2;
/// One ffmpeg run, from waiting for a free slot to the last JPEG byte.
const TIMEOUT: Duration = Duration::from_secs(8);
/// No thumbnail is bigger than this.
const MAX_JPEG: usize = 4 * 1024 * 1024;
/// Slots unused this long are forgotten.
const IDLE: Duration = Duration::from_secs(60);

/// Why there is no picture, and what to answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThumbError {
    /// The stream has no video track, or none ffmpeg is fed here.
    NoVideo(String),
    /// Video, but no keyframe yet.
    NoKeyframe,
    /// ffmpeg is missing, failed, timed out, or all slots stayed busy.
    Unavailable(String),
}

impl IntoResponse for ThumbError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            ThumbError::NoVideo(m) => (StatusCode::CONFLICT, m),
            ThumbError::NoKeyframe => {
                let body = Json(serde_json::json!({ "error": "no video keyframe yet; try again shortly" }));
                return (StatusCode::SERVICE_UNAVAILABLE, [(header::RETRY_AFTER, "1")], body).into_response();
            }
            ThumbError::Unavailable(m) => (StatusCode::SERVICE_UNAVAILABLE, m),
        };
        (status, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}

/// Which keyframe a picture shows: the stream instance plus the frame.
#[derive(Clone)]
struct KeyId {
    stream: Weak<Stream>,
    dts: i64,
    len: usize,
}

impl KeyId {
    fn same(&self, other: &KeyId) -> bool {
        self.stream.ptr_eq(&other.stream) && self.dts == other.dts && self.len == other.len
    }
}

struct Made {
    key: KeyId,
    at: Instant,
    result: Result<Bytes, ThumbError>,
}

struct Slot {
    /// Held while a picture is being made: the single flight.
    made: tokio::sync::Mutex<Option<Made>>,
    used: parking_lot::Mutex<Instant>,
}

/// The thumbnail service: ffmpeg path, slots, the global run limit.
pub struct Thumbnailer {
    ffmpeg: std::sync::OnceLock<PathBuf>,
    slots: parking_lot::Mutex<HashMap<(String, u32), Arc<Slot>>>,
    runs: tokio::sync::Semaphore,
    /// ffmpeg processes started so far (tests check the cache with it).
    started: AtomicU64,
}

impl Default for Thumbnailer {
    fn default() -> Self {
        Self {
            ffmpeg: std::sync::OnceLock::new(),
            slots: Default::default(),
            runs: tokio::sync::Semaphore::new(MAX_RUNS),
            started: AtomicU64::new(0),
        }
    }
}

impl Thumbnailer {
    /// The ffmpeg to run (`[transcode] ffmpeg`). Set once at startup;
    /// `ffmpeg` on `PATH` until then.
    pub fn set_ffmpeg(&self, path: PathBuf) {
        let _ = self.ffmpeg.set(path);
    }

    fn ffmpeg(&self) -> &Path {
        self.ffmpeg.get().map(PathBuf::as_path).unwrap_or(Path::new("ffmpeg"))
    }

    #[cfg(test)]
    pub(crate) fn started(&self) -> u64 {
        self.started.load(Ordering::Relaxed)
    }

    fn slot(&self, stream: &str, width: u32) -> Arc<Slot> {
        let mut slots = self.slots.lock();
        let now = Instant::now();
        // Forget idle slots nobody is using right now.
        slots.retain(|_, s| Arc::strong_count(s) > 1 || now.duration_since(*s.used.lock()) < IDLE);
        let slot = slots
            .entry((stream.to_owned(), width))
            .or_insert_with(|| Arc::new(Slot { made: Default::default(), used: parking_lot::Mutex::new(now) }))
            .clone();
        *slot.used.lock() = now;
        slot
    }

    /// A JPEG `width` pixels wide (never wider than the source) of the
    /// stream's newest video keyframe.
    pub async fn thumbnail(&self, stream: &Arc<Stream>, width: u32) -> Result<Bytes, ThumbError> {
        let (info, frame) = newest_keyframe(stream)?;
        let key = KeyId { stream: Arc::downgrade(stream), dts: frame.dts, len: frame.data.len() };
        let slot = self.slot(stream.name(), width);
        let mut made = slot.made.lock().await;
        if let Some(m) = made.as_ref()
            && m.key.stream.ptr_eq(&key.stream)
            && (m.at.elapsed() < FRESH || m.key.same(&key))
        {
            return m.result.clone();
        }
        // Waited for another request's run: look again at what is newest.
        let (info, frame) = match newest_keyframe(stream) {
            Ok(newer) => newer,
            Err(_) => (info, frame),
        };
        let key = KeyId { stream: Arc::downgrade(stream), dts: frame.dts, len: frame.data.len() };
        let result = self.render(stream.name(), &info, &frame, width).await;
        if let Err(e) = &result {
            tracing::warn!(stream = %stream.name(), error = ?e, "thumbnail failed");
        }
        *made = Some(Made { key, at: Instant::now(), result: result.clone() });
        result
    }

    async fn render(&self, name: &str, info: &TrackInfo, frame: &Frame, width: u32) -> Result<Bytes, ThumbError> {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        let _permit = tokio::time::timeout_at(deadline, self.runs.acquire())
            .await
            .map_err(|_| ThumbError::Unavailable("thumbnail busy; try again shortly".into()))?
            .expect("semaphore never closed");
        let ts = mux_keyframe(info, frame);
        let ffmpeg = self.ffmpeg();
        let args = ffmpeg_args(width);
        self.started.fetch_add(1, Ordering::Relaxed);
        let (proc, mut stdin, mut stdout) =
            caudal_transcode::pipe::FfmpegProcess::spawn(ffmpeg, &args, name).map_err(|e| {
                ThumbError::Unavailable(if e.kind() == std::io::ErrorKind::NotFound {
                    format!("ffmpeg not found at `{}`; install it or set [transcode] ffmpeg", ffmpeg.display())
                } else {
                    format!("could not start ffmpeg `{}`: {e}", ffmpeg.display())
                })
            })?;
        let write = async move {
            // ffmpeg may exit before reading it all; its output says why.
            let _ = stdin.write_all(&ts).await;
            let _ = stdin.shutdown().await;
        };
        let read = async {
            let mut jpeg = Vec::new();
            let mut limited = (&mut stdout).take(MAX_JPEG as u64 + 1);
            limited.read_to_end(&mut jpeg).await.map(|_| jpeg)
        };
        let out = tokio::time::timeout_at(deadline, async { tokio::join!(write, read).1 }).await;
        let tail = proc.shutdown().await;
        let why = || match tail.last() {
            Some(line) => format!(": {line}"),
            None => String::new(),
        };
        match out {
            Err(_) => Err(ThumbError::Unavailable(format!("ffmpeg timed out after {}s", TIMEOUT.as_secs()))),
            Ok(Err(e)) => Err(ThumbError::Unavailable(format!("reading ffmpeg's output failed: {e}"))),
            Ok(Ok(jpeg)) if jpeg.len() > MAX_JPEG => Err(ThumbError::Unavailable("ffmpeg's picture is too big".into())),
            Ok(Ok(jpeg)) if !jpeg.starts_with(&[0xFF, 0xD8]) => {
                Err(ThumbError::Unavailable(format!("ffmpeg made no picture{}", why())))
            }
            Ok(Ok(jpeg)) => Ok(Bytes::from(jpeg)),
        }
    }
}

/// Decode one MPEG-TS from stdin, keep the first picture, scale it to
/// `width` (never up; height keeps the aspect, rounded to even), one JPEG
/// on stdout.
fn ffmpeg_args(width: u32) -> Vec<String> {
    [
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "mpegts",
        "-i",
        "pipe:0",
        "-map",
        "0:v:0",
        "-frames:v",
        "1",
        "-vf",
        &format!("scale=w='min({width},iw)':h=-2"),
        "-c:v",
        "mjpeg",
        "-q:v",
        "5",
        "-f",
        "image2pipe",
        "pipe:1",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// The stream's newest video keyframe and its track.
fn newest_keyframe(stream: &Arc<Stream>) -> Result<(TrackInfo, Arc<Frame>), ThumbError> {
    let tracks = stream.tracks();
    let Some(video) = tracks.iter().find(|t| t.kind() == TrackKind::Video) else {
        return Err(ThumbError::NoVideo("the stream has no video track".into()));
    };
    if !matches!(video.codec, Codec::H264 | Codec::H265 | Codec::Mpeg2Video) {
        return Err(ThumbError::NoVideo(format!(
            "no thumbnails for {} video (H.264, H.265 and MPEG-2 only)",
            video.codec.as_str()
        )));
    }
    let mut sub = stream.subscribe_internal(StartAt::LiveEdge);
    // The live edge starts on the newest keyframe; a few events (the track
    // list, a cue) can come first.
    for _ in 0..64 {
        match sub.try_recv() {
            Some(Event::Frame(f)) if f.track == video.id && f.keyframe => {
                let info = sub.tracks().into_iter().find(|t| t.id == f.track).unwrap_or_else(|| video.clone());
                return Ok((info, f));
            }
            Some(Event::TracksChanged | Event::Cue(_) | Event::Lagged { .. } | Event::Frame(_)) => {}
            Some(Event::End) | None => break,
        }
    }
    Err(ThumbError::NoKeyframe)
}

/// One keyframe as a complete MPEG-TS: PAT, PMT, the frame with its
/// parameter sets / sequence header.
fn mux_keyframe(info: &TrackInfo, frame: &Frame) -> Vec<u8> {
    let mut mux = caudal_ts::mux::TsMux::new();
    mux.set_tracks(std::slice::from_ref(info));
    mux.push_frame(info, frame);
    mux.take_output()
}

/// The HTTP answer for a picture.
pub fn jpeg_response(jpeg: Bytes) -> Response {
    let cache = HeaderValue::from_str(&format!("private, max-age={MAX_AGE_SECS}")).expect("ascii");
    ([(header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg")), (header::CACHE_CONTROL, cache)], jpeg)
        .into_response()
}

/// `?width=` checked: [`DEFAULT_WIDTH`] when absent.
pub fn parse_width(width: Option<u32>) -> Result<u32, String> {
    match width {
        None => Ok(DEFAULT_WIDTH),
        Some(w) if (MIN_WIDTH..=MAX_WIDTH).contains(&w) => Ok(w),
        Some(_) => Err(format!("width must be {MIN_WIDTH} to {MAX_WIDTH}")),
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use axum::body::Body;
    use axum::http::Request;
    use caudal_core::{AudioParams, BufferConfig, Publisher, Registry, TrackId, VideoParams};
    use caudal_ts::demux::{DemuxEvent, Demuxer};
    use caudal_ts::ts::TsDemux;
    use tower::ServiceExt;

    use super::*;
    use crate::api::{AppState, router};

    fn have(tool: &str) -> bool {
        Command::new(tool).arg("-version").output().is_ok_and(|o| o.status.success())
    }

    /// Real ffmpeg and ffprobe, or the test says it is skipped.
    fn tools() -> bool {
        let ok = have("ffmpeg") && have("ffprobe");
        if !ok {
            eprintln!("SKIP: ffmpeg/ffprobe not on PATH");
        }
        ok
    }

    /// Two seconds of 640x360 `testsrc` encoded with `codecs`, through
    /// MPEG-TS and `caudal-ts`, published as `name`.
    fn publish_clip(reg: &Arc<Registry>, name: &str, codecs: &[&str]) -> Publisher {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("src.ts");
        let st = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=640x360:rate=30"])
            .args(["-t", "2"])
            .args(codecs)
            .args(["-f", "mpegts"])
            .arg(&path)
            .status()
            .unwrap();
        assert!(st.success());
        let mut ts = TsDemux::new();
        ts.feed(&std::fs::read(&path).unwrap());
        let mut units = Vec::new();
        ts.drain(&mut units);
        let (mut demux, mut events) = (Demuxer::new(), Vec::new());
        for u in units {
            demux.consume(u, &mut events);
        }
        let publisher = reg.publish(name, BufferConfig::default()).unwrap();
        let mut tracks = Vec::new();
        let mut frames = Vec::new();
        for ev in events {
            match ev {
                DemuxEvent::VideoInit(t) if tracks.is_empty() => tracks.push(t),
                DemuxEvent::VideoFrame(f) => frames.push(f),
                _ => {}
            }
        }
        assert_eq!(tracks.len(), 1, "one video track");
        publisher.set_tracks(tracks).unwrap();
        let keyframes = frames.iter().filter(|f| f.keyframe).count();
        assert!(keyframes >= 2, "several GOPs, so the newest keyframe is not the first: {keyframes}");
        for f in frames {
            publisher.push(f).unwrap();
        }
        publisher
    }

    async fn get(app: &axum::Router, uri: &str) -> (StatusCode, axum::http::HeaderMap, Bytes) {
        let res = app.clone().oneshot(Request::get(uri).body(Body::empty()).unwrap()).await.unwrap();
        let (status, headers) = (res.status(), res.headers().clone());
        (status, headers, axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap())
    }

    /// Width and height of a JPEG, read by ffprobe.
    fn probe(jpeg: &[u8]) -> (u32, u32) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jpg");
        std::fs::write(&path, jpeg).unwrap();
        let out = Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=codec_name,width,height"])
            .args(["-of", "csv=p=0"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let text = String::from_utf8(out.stdout).unwrap();
        let parts: Vec<&str> = text.trim().split(',').collect();
        assert_eq!(parts[0], "mjpeg", "{text}");
        (parts[1].parse().unwrap(), parts[2].parse().unwrap())
    }

    fn is_jpeg(b: &[u8]) -> bool {
        b.starts_with(&[0xFF, 0xD8, 0xFF]) && b.ends_with(&[0xFF, 0xD9])
    }

    const H264: &[&str] = &["-c:v", "libx264", "-preset", "ultrafast", "-g", "15", "-bf", "2", "-pix_fmt", "yuv420p"];

    #[tokio::test]
    async fn h264_thumbnail_is_a_scaled_jpeg_and_is_cached() {
        if !tools() {
            return;
        }
        let state = AppState::new(Registry::new());
        let app = router(state.clone());
        let _p = publish_clip(&state.registry, "cam", H264);

        let (status, headers, body) = get(&app, "/api/v1/streams/cam/thumbnail.jpg").await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        assert_eq!(headers[header::CONTENT_TYPE], "image/jpeg");
        assert_eq!(headers[header::CACHE_CONTROL], "private, max-age=2");
        assert!(is_jpeg(&body));
        assert_eq!(probe(&body), (DEFAULT_WIDTH, 180), "default width, aspect kept");
        assert_eq!(state.thumbnails.started(), 1);

        // Same keyframe: same bytes, no second ffmpeg.
        let (status, _, again) = get(&app, "/api/v1/streams/cam/thumbnail.jpg").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(again, body);
        assert_eq!(state.thumbnails.started(), 1, "served from the cache");

        // Many viewers of a new size at once: one ffmpeg between them.
        let reqs = (0..8).map(|_| {
            let app = app.clone();
            async move { get(&app, "/api/v1/streams/cam/thumbnail.jpg?width=200").await }
        });
        let all = futures_join_all(reqs).await;
        assert_eq!(state.thumbnails.started(), 2, "single flight");
        for (status, _, b) in &all {
            assert_eq!(*status, StatusCode::OK);
            assert_eq!(b, &all[0].2);
        }
        let (w, h) = probe(&all[0].2);
        assert_eq!(w, 200);
        assert!((112..=114).contains(&h), "aspect kept: {h}");

        // Never upscaled past the source.
        let (_, _, big) = get(&app, "/api/v1/streams/cam/thumbnail.jpg?width=1280").await;
        assert_eq!(probe(&big), (640, 360));
    }

    async fn futures_join_all<F>(futs: impl Iterator<Item = F>) -> Vec<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let handles: Vec<_> = futs.map(tokio::spawn).collect();
        let mut out = Vec::new();
        for h in handles {
            out.push(h.await.unwrap());
        }
        out
    }

    #[tokio::test]
    async fn mpeg2_and_h265_sources_work() {
        if !tools() {
            return;
        }
        let state = AppState::new(Registry::new());
        let app = router(state.clone());
        let _m = publish_clip(
            &state.registry,
            "tv",
            &["-c:v", "mpeg2video", "-b:v", "3M", "-g", "15", "-bf", "2", "-flags", "+ildct+ilme"],
        );
        let (status, _, body) = get(&app, "/api/v1/streams/tv/thumbnail.jpg?width=160").await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        assert!(is_jpeg(&body));
        assert_eq!(probe(&body), (160, 90));

        let encoders = Command::new("ffmpeg").args(["-hide_banner", "-encoders"]).output().unwrap();
        if !String::from_utf8_lossy(&encoders.stdout).contains("libx265") {
            eprintln!("SKIP h265: no libx265 in this ffmpeg");
            return;
        }
        let _h = publish_clip(
            &state.registry,
            "hevc",
            &["-c:v", "libx265", "-preset", "ultrafast", "-x265-params", "keyint=15:log-level=error"],
        );
        let (status, _, body) = get(&app, "/api/v1/streams/hevc/thumbnail.jpg").await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        assert_eq!(probe(&body), (320, 180));
    }

    fn video_track() -> TrackInfo {
        TrackInfo {
            id: TrackId(0),
            codec: Codec::H264,
            timescale: 90_000,
            init: Default::default(),
            lang: None,
            video: Some(VideoParams { width: 640, height: 360, fps: None }),
            audio: None,
        }
    }

    /// Everything that is answered before ffmpeg runs, and a missing
    /// ffmpeg; none of it needs ffmpeg installed.
    #[tokio::test]
    async fn errors_are_clear() {
        let state = AppState::new(Registry::new());
        state.thumbnails.set_ffmpeg(PathBuf::from("/nonexistent/ffmpeg-for-caudal-tests"));
        let app = router(state.clone());

        let (status, _, _) = get(&app, "/api/v1/streams/nope/thumbnail.jpg").await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let audio = state.registry.publish("radio", BufferConfig::default()).unwrap();
        audio
            .set_tracks(vec![TrackInfo {
                id: TrackId(1),
                codec: Codec::Aac,
                timescale: 48_000,
                init: Bytes::from_static(&[0x11, 0x90]),
                lang: None,
                video: None,
                audio: Some(AudioParams { sample_rate: 48_000, channels: 2 }),
            }])
            .unwrap();
        let (status, _, body) = get(&app, "/api/v1/streams/radio/thumbnail.jpg").await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(String::from_utf8_lossy(&body).contains("no video"), "{}", String::from_utf8_lossy(&body));

        let cam = state.registry.publish("cam", BufferConfig::default()).unwrap();
        cam.set_tracks(vec![video_track()]).unwrap();
        let (status, headers, _) = get(&app, "/api/v1/streams/cam/thumbnail.jpg").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "no keyframe yet");
        assert_eq!(headers[header::RETRY_AFTER], "1");

        let (status, _, _) = get(&app, "/api/v1/streams/cam/thumbnail.jpg?width=5000").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        cam.push(Frame {
            track: TrackId(0),
            dts: 0,
            pts: 0,
            keyframe: true,
            data: Bytes::from_static(&[0, 0, 0, 1, 0x65]),
        })
        .unwrap();
        let (status, _, body) = get(&app, "/api/v1/streams/cam/thumbnail.jpg").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains("ffmpeg not found"), "{body}");
    }
}
