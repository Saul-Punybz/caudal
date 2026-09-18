//! Integration tests: real `ffmpeg` publishing real RTMP to a real
//! `caudal_rtmp::serve` listener, backed by a real `caudal_core::Registry`.
//!
//! Every test that needs `ffmpeg` checks for it first and prints "SKIP"
//! instead of failing when it's missing, per the project's testing rules.

use std::net::TcpListener as StdTcpListener;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use caudal_core::{BufferConfig, Codec, Event, Registry, StartAt, Stream, TrackKind};
use caudal_rtmp::{RtmpConfig, serve};

fn have_ffmpeg() -> bool {
    Command::new("which").arg("ffmpeg").output().is_ok_and(|o| o.status.success())
}

fn have_libx265() -> bool {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("libx265"))
}

fn free_port() -> u16 {
    StdTcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// Starts `caudal_rtmp::serve` on a free port with a fresh registry.
/// Dropping the returned join handle's abort guard stops the listener.
struct TestServer {
    pub registry: Arc<Registry>,
    pub port: u16,
    handle: tokio::task::JoinHandle<()>,
}

impl TestServer {
    async fn start(app: &str) -> Self {
        let port = free_port();
        let registry = Registry::new();
        let cfg = RtmpConfig {
            bind: format!("127.0.0.1:{port}").parse().unwrap(),
            app: app.to_string(),
            buffer: BufferConfig::default(),
        };
        let reg = registry.clone();
        let handle = tokio::spawn(async move {
            let _ = serve(cfg, reg).await;
        });
        // Let the listener bind before ffmpeg tries to connect.
        wait_for(Duration::from_secs(2), || std::net::TcpStream::connect(("127.0.0.1", port)).ok().map(|_| ()))
            .await
            .expect("rtmp listener never came up");
        Self { registry, port, handle }
    }

    fn rtmp_url(&self, app: &str, name: &str) -> String {
        format!("rtmp://127.0.0.1:{}/{app}/{name}", self.port)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Polls `f` every 50ms until it returns `Some`, or gives up after `timeout`.
async fn wait_for<T>(timeout: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(v) = f() {
            return Some(v);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Polls `child.try_wait()` until it exits or `timeout` elapses.
async fn wait_for_exit(child: &mut std::process::Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    wait_for(timeout, || child.try_wait().ok().flatten()).await
}

fn spawn_ffmpeg(args: &[&str]) -> std::process::Child {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-re"])
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn ffmpeg")
}

/// True when `data` looks like AVCC: a 4-byte big-endian length prefix whose
/// value fits within the remaining bytes.
fn looks_like_avcc(data: &[u8]) -> bool {
    if data.len() < 4 {
        return false;
    }
    let len = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    len <= data.len() - 4
}

async fn wait_for_tracks(stream: &Arc<Stream>, timeout: Duration) -> Vec<caudal_core::TrackInfo> {
    wait_for(timeout, || {
        let tracks = stream.tracks();
        (!tracks.is_empty()).then_some(tracks)
    })
    .await
    .expect("tracks were never announced")
}

#[tokio::test(flavor = "multi_thread")]
async fn h264_aac_publish_end_to_end() {
    if !have_ffmpeg() {
        eprintln!("SKIP: ffmpeg not installed");
        return;
    }

    let server = TestServer::start("live").await;
    let url = server.rtmp_url("live", "test");

    let mut ffmpeg = spawn_ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=1280x720:rate=30",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=48000",
        "-c:v",
        "libx264",
        "-g",
        "60",
        "-c:a",
        "aac",
        "-t",
        "4",
        "-f",
        "flv",
        &url,
    ]);

    let stream = wait_for(Duration::from_secs(10), || server.registry.get("test"))
        .await
        .expect("stream 'test' never appeared in the registry");

    let tracks = wait_for_tracks(&stream, Duration::from_secs(5)).await;
    assert_eq!(tracks.len(), 2, "expected exactly one video and one audio track: {tracks:?}");

    let video = tracks.iter().find(|t| t.kind() == TrackKind::Video).expect("no video track");
    let audio = tracks.iter().find(|t| t.kind() == TrackKind::Audio).expect("no audio track");

    assert_eq!(video.codec, Codec::H264);
    assert_eq!(audio.codec, Codec::Aac);

    let vp = video.video.expect("video track missing VideoParams");
    assert_eq!((vp.width, vp.height), (1280, 720), "wrong dimensions parsed from SPS");

    let ap = audio.audio.expect("audio track missing AudioParams");
    assert_eq!(ap.sample_rate, 48_000, "wrong sample rate parsed from AudioSpecificConfig");

    // Frames flow, the first video frame is a keyframe, and its data is AVCC.
    let mut sub = stream.subscribe(StartAt::Oldest);
    let mut saw_first_video = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline && !saw_first_video {
        match tokio::time::timeout(Duration::from_millis(500), sub.recv()).await {
            Ok(Event::Frame(frame)) if frame.track == caudal_core::TrackId(0) => {
                assert!(frame.keyframe, "first video frame should be a keyframe");
                assert!(looks_like_avcc(&frame.data), "video frame is not AVCC-prefixed");
                saw_first_video = true;
            }
            Ok(_) => continue,
            Err(_) => continue, // per-recv timeout; keep trying until the outer deadline
        }
    }
    assert!(saw_first_video, "never observed a video frame");

    // Let it run a bit more so frames_in comfortably passes 60 (4s @ 30fps).
    wait_for(Duration::from_secs(6), || (stream.stats().frames_in > 60).then_some(()))
        .await
        .expect("frames_in never exceeded 60");

    // ffmpeg finishes on its own after -t 4; wait for a clean exit, then the
    // stream must disappear within 5s.
    let _ = ffmpeg.wait();
    let gone = wait_for(Duration::from_secs(5), || server.registry.get("test").is_none().then_some(())).await;
    assert!(gone.is_some(), "stream 'test' was not removed within 5s of ffmpeg exiting");
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_app_is_rejected() {
    if !have_ffmpeg() {
        eprintln!("SKIP: ffmpeg not installed");
        return;
    }

    let server = TestServer::start("live").await;
    let url = server.rtmp_url("wrong", "test");

    let mut ffmpeg = spawn_ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=320x240:rate=15",
        "-f",
        "lavfi",
        "-i",
        "sine",
        "-c:v",
        "libx264",
        "-g",
        "30",
        "-c:a",
        "aac",
        "-t",
        "2",
        "-f",
        "flv",
        &url,
    ]);

    let status = wait_for_exit(&mut ffmpeg, Duration::from_secs(3))
        .await
        .expect("ffmpeg should be disconnected and exit within 3s of publishing to the wrong app");
    assert!(!status.success(), "ffmpeg should exit non-zero when its publish is rejected");
    assert!(server.registry.get("test").is_none(), "publish to the wrong app must not create a stream");
    assert!(server.registry.list().is_empty(), "publish to the wrong app must not create any stream");
}

#[tokio::test(flavor = "multi_thread")]
async fn busy_name_second_publisher_is_rejected() {
    if !have_ffmpeg() {
        eprintln!("SKIP: ffmpeg not installed");
        return;
    }

    let server = TestServer::start("live").await;
    let url = server.rtmp_url("live", "same");

    let mut first = spawn_ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=320x240:rate=15",
        "-f",
        "lavfi",
        "-i",
        "sine",
        "-c:v",
        "libx264",
        "-g",
        "30",
        "-c:a",
        "aac",
        "-t",
        "8",
        "-f",
        "flv",
        &url,
    ]);

    let stream = wait_for(Duration::from_secs(10), || server.registry.get("same"))
        .await
        .expect("first publisher's stream never appeared in the registry");

    wait_for(Duration::from_secs(5), || (stream.stats().frames_in > 0).then_some(()))
        .await
        .expect("first publisher never produced any frames");

    // Give the first publisher a head start before the second races in.
    tokio::time::sleep(Duration::from_secs(1)).await;

    let mut second = spawn_ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=320x240:rate=15",
        "-f",
        "lavfi",
        "-i",
        "sine",
        "-c:v",
        "libx264",
        "-g",
        "30",
        "-c:a",
        "aac",
        "-t",
        "4",
        "-f",
        "flv",
        &url,
    ]);

    let status = wait_for_exit(&mut second, Duration::from_secs(3))
        .await
        .expect("the second (busy-name) publisher should be disconnected and exit within 3s");
    assert!(!status.success(), "the busy second publisher should exit non-zero when rejected");

    // The first publisher must be unaffected: frames_in keeps rising for 2 more seconds.
    let before = stream.stats().frames_in;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let after = stream.stats().frames_in;
    assert!(
        after > before,
        "first publisher's frames_in should keep rising after the second was rejected ({before} -> {after})"
    );

    let _ = first.kill();
    let _ = first.wait();
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_stream_name_is_rejected() {
    if !have_ffmpeg() {
        eprintln!("SKIP: ffmpeg not installed");
        return;
    }

    let server = TestServer::start("live").await;
    let url = server.rtmp_url("live", "..bad");

    let mut ffmpeg = spawn_ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=320x240:rate=15",
        "-f",
        "lavfi",
        "-i",
        "sine",
        "-c:v",
        "libx264",
        "-g",
        "30",
        "-c:a",
        "aac",
        "-t",
        "2",
        "-f",
        "flv",
        &url,
    ]);

    let status = wait_for_exit(&mut ffmpeg, Duration::from_secs(3))
        .await
        .expect("ffmpeg should be disconnected and exit within 3s of an invalid stream name publish");
    assert!(!status.success(), "ffmpeg should exit non-zero when the stream name is rejected");
    assert!(server.registry.list().is_empty(), "an invalid stream name must not create a stream");
}

#[tokio::test(flavor = "multi_thread")]
async fn hevc_publish_end_to_end() {
    if !have_ffmpeg() || !have_libx265() {
        eprintln!("SKIP: ffmpeg without libx265");
        return;
    }

    let server = TestServer::start("live").await;
    let url = server.rtmp_url("live", "hevctest");

    let mut ffmpeg = spawn_ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=640x480:rate=30",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=48000",
        "-c:v",
        "libx265",
        "-g",
        "60",
        "-c:a",
        "aac",
        "-t",
        "3",
        "-f",
        "flv",
        &url,
    ]);

    let stream = wait_for(Duration::from_secs(10), || server.registry.get("hevctest"))
        .await
        .expect("stream 'hevctest' never appeared in the registry");

    let tracks = wait_for_tracks(&stream, Duration::from_secs(5)).await;
    let video = tracks.iter().find(|t| t.kind() == TrackKind::Video).expect("no video track");
    assert_eq!(video.codec, Codec::H265, "expected Enhanced RTMP HEVC to map to Codec::H265");

    let _ = ffmpeg.kill();
    let _ = ffmpeg.wait();
}
