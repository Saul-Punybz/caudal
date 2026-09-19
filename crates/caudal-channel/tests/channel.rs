//! Channel behaviour against the MP4 fixtures of caudal-hls (4 s each,
//! H.264 256x144 30 fps with B-frames, plus AAC or Opus at 48 kHz). Tokio
//! time is paused, so hours of pacing run in milliseconds.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use caudal_channel::{Channel, ChannelConfig, ChannelHandle, ChannelState};
use caudal_core::{Event, Frame, Registry, StartAt, Stream, TrackInfo, TrackKind};
use tower::ServiceExt;

const VIDEO_FRAMES_PER_FILE: usize = 120;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../caudal-hls/tests/fixtures").join(name)
}

fn channel(name: &str, items: Vec<PathBuf>, r#loop: bool) -> Channel {
    Channel { name: name.into(), items, r#loop, shuffle: false }
}

/// Starts one channel and returns its stream once it is published.
async fn start(ch: Channel) -> (Arc<Registry>, ChannelHandle, Arc<Stream>) {
    let registry = Registry::new();
    let mut publishes = registry.subscribe_publishes();
    let name = ch.name.clone();
    let handle = caudal_channel::start(
        registry.clone(),
        ChannelConfig { channels: vec![ch], buffer: Default::default(), trusted_proxies: Vec::new() },
    );
    let stream = loop {
        let s = tokio::time::timeout(Duration::from_secs(30), publishes.recv()).await.expect("publish").unwrap();
        if s.name() == name {
            break s;
        }
    };
    (registry, handle, stream)
}

struct Received {
    tracks: Vec<TrackInfo>,
    frames: Vec<Frame>,
    ended: bool,
}

impl Received {
    fn video(&self) -> Vec<&Frame> {
        let id = self.tracks.iter().find(|t| t.kind() == TrackKind::Video).unwrap().id;
        self.frames.iter().filter(|f| f.track == id).collect()
    }

    fn assert_monotonic(&self) {
        for t in &self.tracks {
            let dts: Vec<i64> = self.frames.iter().filter(|f| f.track == t.id).map(|f| f.dts).collect();
            assert!(!dts.is_empty(), "track {:?} got no frames", t.id);
            for w in dts.windows(2) {
                assert!(w[1] > w[0], "track {:?}: dts {} then {}", t.id, w[0], w[1]);
            }
        }
    }
}

/// Reads from the oldest keyframe until the stream ends or `stop` says so.
async fn read(stream: &Arc<Stream>, mut stop: impl FnMut(&Received) -> bool) -> Received {
    let mut sub = stream.subscribe_internal(StartAt::Oldest);
    let mut r = Received { tracks: sub.tracks(), frames: Vec::new(), ended: false };
    loop {
        match tokio::time::timeout(Duration::from_secs(60), sub.recv()).await.expect("stalled") {
            Event::TracksChanged => r.tracks = sub.tracks(),
            Event::Frame(f) => {
                r.frames.push((*f).clone());
                if stop(&r) {
                    return r;
                }
            }
            Event::Lagged { skipped } => panic!("lagged {skipped}"),
            Event::Cue(_) => {}
            Event::End => {
                r.ended = true;
                return r;
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn two_files_back_to_back() {
    let ch = channel("two", vec![fixture("av.mp4"), fixture("av.mp4")], false);
    let (_reg, handle, stream) = start(ch).await;
    let r = read(&stream, |_| false).await;
    assert!(r.ended, "loop=false ends the stream");
    assert_eq!(r.tracks.len(), 2);
    r.assert_monotonic();
    let video = r.video();
    assert_eq!(video.len(), 2 * VIDEO_FRAMES_PER_FILE);
    assert!(video[0].keyframe);
    let second = video[VIDEO_FRAMES_PER_FILE];
    assert!(second.keyframe, "second file starts on a keyframe");
    // It continues right after the first file: within one audio frame.
    let end_first = video[VIDEO_FRAMES_PER_FILE - 1].dts + 3000;
    assert!(second.dts >= end_first && second.dts - end_first < 90_000 / 20, "{} vs {end_first}", second.dts);
    let s = &handle.status()[0];
    assert_eq!(s.state, ChannelState::Idle);
    assert!(s.now_playing.is_none());
    assert!(s.error.is_none());
}

#[tokio::test(start_paused = true)]
async fn loop_wraps_with_monotonic_timestamps() {
    let ch = channel("looped", vec![fixture("av.mp4")], true);
    let (_reg, handle, stream) = start(ch).await;
    let want = 3 * VIDEO_FRAMES_PER_FILE + 1;
    let r = read(&stream, |r| r.video().len() >= want).await;
    handle.stop();
    r.assert_monotonic();
    let video = r.video();
    for pass in 1..=3 {
        assert!(video[pass * VIDEO_FRAMES_PER_FILE].keyframe, "pass {pass} starts on a keyframe");
    }
    // Three passes of 4 s (plus the audio tail per pass) on the 90 kHz clock.
    let span = video[3 * VIDEO_FRAMES_PER_FILE].dts - video[0].dts;
    assert!((3 * 360_000..3 * 368_000).contains(&span), "span {span}");
}

#[tokio::test(start_paused = true)]
async fn mismatched_file_is_skipped() {
    let ch = channel(
        "mixed",
        vec![fixture("av.mp4"), fixture("av_opus.mp4"), PathBuf::from("/nonexistent/x.mp4"), fixture("av.mp4")],
        false,
    );
    let (_reg, handle, stream) = start(ch).await;
    let r = read(&stream, |_| false).await;
    assert!(r.ended);
    r.assert_monotonic();
    assert_eq!(r.video().len(), 2 * VIDEO_FRAMES_PER_FILE, "both AAC files played, the Opus one skipped");
    let audio = r.tracks.iter().find(|t| t.kind() == TrackKind::Audio).unwrap();
    assert_eq!(audio.codec, caudal_core::Codec::Aac);
    let s = &handle.status()[0];
    let err = s.error.as_deref().expect("error kept");
    assert!(err.contains("x.mp4"), "last error is the missing file: {err}");
}

#[tokio::test(start_paused = true)]
async fn mismatch_error_is_reported() {
    let ch = channel("mixed2", vec![fixture("av.mp4"), fixture("av_opus.mp4")], false);
    let (_reg, handle, stream) = start(ch).await;
    let r = read(&stream, |_| false).await;
    assert!(r.ended);
    let err = handle.status()[0].error.clone().expect("error");
    assert!(err.contains("av_opus.mp4") && err.contains("differs"), "{err}");
}

#[tokio::test(start_paused = true)]
async fn paced_in_real_time() {
    let ch = channel("paced", vec![fixture("av.mp4")], false);
    let (_reg, handle, stream) = start(ch).await;
    let t = tokio::time::Instant::now();
    let r = read(&stream, |r| r.video().last().is_some_and(|f| f.dts >= 2 * 90_000)).await;
    let elapsed = t.elapsed();
    handle.stop();
    assert!(elapsed >= Duration::from_millis(1800), "2 s of media in {elapsed:?}");
    assert!(elapsed <= Duration::from_millis(2100), "2 s of media in {elapsed:?}");
    assert!(!r.ended);
}

#[tokio::test(start_paused = true)]
async fn nothing_playable_stays_idle() {
    let ch = channel("empty", vec![PathBuf::from("/nonexistent/a.mp4")], true);
    let registry = Registry::new();
    let handle = caudal_channel::start(
        registry.clone(),
        ChannelConfig { channels: vec![ch], buffer: Default::default(), trusted_proxies: Vec::new() },
    );
    tokio::time::sleep(Duration::from_secs(25)).await;
    let s = &handle.status()[0];
    assert_eq!(s.state, ChannelState::Idle);
    assert!(s.error.as_deref().is_some_and(|e| e.contains("open")), "{:?}", s.error);
    assert!(registry.get("empty").is_none());
    handle.stop();
}

#[tokio::test(start_paused = true)]
async fn name_already_published_waits() {
    let registry = Registry::new();
    let taken = registry.publish("busy", Default::default()).unwrap();
    let ch = channel("busy", vec![fixture("av.mp4")], true);
    let handle = caudal_channel::start(
        registry.clone(),
        ChannelConfig { channels: vec![ch], buffer: Default::default(), trusted_proxies: Vec::new() },
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
    let s = handle.status()[0].clone();
    assert!(s.error.as_deref().is_some_and(|e| e.contains("already published")), "{s:?}");
    drop(taken);
    tokio::time::sleep(Duration::from_secs(11)).await;
    assert_eq!(handle.status()[0].state, ChannelState::Playing);
    handle.stop();
}

async fn get_json(app: &axum::Router) -> serde_json::Value {
    let res = app.clone().oneshot(Request::get("/api/v1/channels").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test(start_paused = true)]
async fn router_lists_and_skips() {
    // A directory item: its media files, sorted by name.
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(fixture("av.mp4"), dir.path().join("a.mp4")).unwrap();
    std::fs::copy(fixture("av.mp4"), dir.path().join("b.mp4")).unwrap();
    std::fs::write(dir.path().join("notes.txt"), "ignored").unwrap();
    let ch = channel("tv", vec![dir.path().to_owned()], true);
    let (_reg, handle, _stream) = start(ch).await;
    let app = caudal_channel::router(handle.clone());
    tokio::time::sleep(Duration::from_secs(1)).await;

    let v = get_json(&app).await;
    let c = &v[0];
    assert_eq!(c["name"], "tv");
    assert_eq!(c["state"], "playing");
    assert_eq!(c["index"], 0);
    assert_eq!(c["items"], 2);
    assert!(c["error"].is_null());
    let np = &c["now_playing"];
    assert!(np["path"].as_str().unwrap().ends_with("a.mp4"), "{np}");
    let dur = np["duration_secs"].as_f64().unwrap();
    assert!((4.0..4.05).contains(&dur), "duration {dur}");
    let pos = np["position_secs"].as_f64().unwrap();
    assert!((0.8..1.3).contains(&pos), "position {pos}");

    let res =
        app.clone().oneshot(Request::post("/api/v1/channels/tv/skip").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let v = get_json(&app).await;
    assert_eq!(v[0]["index"], 1);
    assert!(v[0]["now_playing"]["path"].as_str().unwrap().ends_with("b.mp4"));
    assert!(v[0]["now_playing"]["position_secs"].as_f64().unwrap() < 0.5);

    let res =
        app.clone().oneshot(Request::post("/api/v1/channels/nope/skip").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    handle.stop();
}

#[tokio::test(start_paused = true)]
async fn skip_keeps_timestamps_monotonic() {
    let ch = channel("skipper", vec![fixture("av.mp4"), fixture("av.mp4")], false);
    let (_reg, handle, stream) = start(ch).await;
    let h = handle.clone();
    let mut skipped = false;
    let r = read(&stream, move |r| {
        if !skipped && r.video().len() == 30 {
            skipped = true;
            h.skip("skipper");
        }
        false
    })
    .await;
    assert!(r.ended);
    r.assert_monotonic();
    let video = r.video();
    // Part of the first file was skipped; the whole second file played.
    assert!(video.len() < 2 * VIDEO_FRAMES_PER_FILE, "{}", video.len());
    assert!(video.len() >= VIDEO_FRAMES_PER_FILE + 30, "{}", video.len());
    let second = &video[video.len() - VIDEO_FRAMES_PER_FILE];
    assert!(second.keyframe, "the next item starts on a keyframe");
}

/// Kills ffmpeg if a test panics while it runs.
struct Ffmpeg(std::process::Child);

impl Drop for Ffmpeg {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn ffmpeg(args: &[&str]) -> bool {
    let Ok(child) = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .stdin(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };
    let mut guard = Ffmpeg(child);
    guard.0.wait().is_ok_and(|s| s.success())
}

/// MPEG-TS items, stitched like MP4 ones. Needs ffmpeg for the fixture.
#[tokio::test(start_paused = true)]
async fn transport_stream_files() {
    let dir = tempfile::tempdir().unwrap();
    let ts = dir.path().join("av.ts");
    let src = fixture("av.mp4");
    if !ffmpeg(&["-i", src.to_str().unwrap(), "-c", "copy", "-f", "mpegts", ts.to_str().unwrap()]) {
        eprintln!("ffmpeg not available; skipping");
        return;
    }
    let ch = channel("ts", vec![ts.clone(), ts], false);
    let (_reg, handle, stream) = start(ch).await;
    let r = read(&stream, |_| false).await;
    assert!(r.ended);
    r.assert_monotonic();
    let video = r.video();
    assert_eq!(video.len(), 2 * VIDEO_FRAMES_PER_FILE, "last frame of each file flushed");
    assert!(video[VIDEO_FRAMES_PER_FILE].keyframe);
    assert!(handle.status()[0].error.is_none(), "{:?}", handle.status()[0].error);
}
