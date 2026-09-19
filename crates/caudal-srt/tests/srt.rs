//! Integration tests: real `ffmpeg` piped through the real `srt-live-transmit`
//! (libsrt 1.5.6) into a real `caudal_srt::serve` listener, backed by a real
//! `caudal_core::Registry`.
//!
//! Local `ffmpeg` has no `srt` protocol built in, so the wire path is
//! `ffmpeg (mpegts on stdout) | srt-live-transmit (libsrt caller) -> caudal_srt (rsrt listener)`,
//! exactly as spelled out in the batch brief.
//!
//! Every test that needs `ffmpeg` or `srt-live-transmit` checks for them
//! first and prints "SKIP" instead of failing when either is missing.

use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use caudal_core::{Access, BufferConfig, Codec, Denied, Event, Gate, GateFuture, Registry, StartAt, Stream, TrackKind};
use caudal_srt::{SrtConfig, SrtPush, serve};

fn have(bin: &str) -> bool {
    Command::new("which").arg(bin).output().is_ok_and(|o| o.status.success())
}

/// SRT runs over UDP, so the probe must be a UDP bind: a TCP probe can
/// hand two parallel tests the same UDP port.
fn free_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// Starts `caudal_srt::serve` on a free UDP port with a fresh registry.
struct TestServer {
    pub registry: Arc<Registry>,
    pub port: u16,
    handle: tokio::task::JoinHandle<()>,
}

impl TestServer {
    async fn start(passphrase: Option<&str>) -> Self {
        Self::start_full(passphrase, None, Vec::new()).await
    }

    async fn start_with_gate(gate: Arc<dyn Gate>) -> Self {
        Self::start_full(None, Some(gate), Vec::new()).await
    }

    async fn start_with_pushes(pushes: Vec<SrtPush>) -> Self {
        Self::start_full(None, None, pushes).await
    }

    async fn start_full(passphrase: Option<&str>, gate: Option<Arc<dyn Gate>>, pushes: Vec<SrtPush>) -> Self {
        let registry = Registry::new();
        if let Some(gate) = gate {
            registry.set_gate(gate);
        }
        // The probed port can be taken by another parallel test's socket
        // before `serve` binds it; `serve` then returns at once with
        // EADDRINUSE. Retry on a fresh port instead of failing later with a
        // confusing "never appeared".
        for _ in 0..5 {
            let port = free_port();
            let cfg = SrtConfig {
                bind: format!("127.0.0.1:{port}").parse().unwrap(),
                latency_ms: 120,
                passphrase: passphrase.map(str::to_owned),
                buffer: BufferConfig::default(),
            };
            let reg = registry.clone();
            let handle = tokio::spawn(async move {
                let _ = serve(cfg, reg).await;
            });
            // Let the listener bind before the pipeline tries to connect.
            tokio::time::sleep(Duration::from_millis(300)).await;
            if !handle.is_finished() {
                if !pushes.is_empty() {
                    caudal_srt::start_pushes(registry.clone(), pushes.clone());
                }
                return Self { registry, port, handle };
            }
        }
        panic!("SRT listener could not bind a free port in 5 tries");
    }
}

/// Denies `Access::Play` unconditionally; publish is always allowed so a
/// stream can exist for the play attempt to be refused against.
struct DenyPlayGate;

impl Gate for DenyPlayGate {
    fn check<'a>(&'a self, access: Access, _stream: &'a str, _token: Option<&'a str>) -> GateFuture<'a> {
        Box::pin(async move {
            match access {
                Access::Play => Err(Denied::Refused("test gate denies play".to_owned())),
                Access::Publish => Ok(()),
            }
        })
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

async fn wait_for_tracks(stream: &Arc<Stream>, timeout: Duration) -> Vec<caudal_core::TrackInfo> {
    wait_for(timeout, || {
        let tracks = stream.tracks();
        (!tracks.is_empty()).then_some(tracks)
    })
    .await
    .expect("tracks were never announced")
}

/// ffmpeg writing MPEG-TS into `srt-live-transmit`, which carries it over
/// SRT to `srt_url`. Homebrew's ffmpeg has no SRT protocol, hence the pipe.
///
/// `srt-live-transmit` does NOT exit when its input ends: it busy-loops at
/// 100% CPU. Five leaked copies overheated the dev laptop on 18 Sep 2026.
/// So the pipeline runs in its own process group and this guard kills the
/// whole group when it goes out of scope, including when a test panics.
/// Never wait for the pipeline to end by itself; drop the guard instead.
struct Pipeline {
    child: Child,
}

impl Pipeline {
    fn start(size: &str, duration_secs: u32, srt_url: &str) -> Self {
        use std::os::unix::process::CommandExt;
        // A short GOP (15 frames @ 30 fps = 0.5 s) keeps the first join point
        // near 0: audio init can arrive a few frames after the first keyframe.
        let cmd = format!(
            "ffmpeg -hide_banner -loglevel error -re -f lavfi -i testsrc2=size={size}:rate=30 \
             -f lavfi -i sine=frequency=440:sample_rate=48000 -c:v libx264 -g 15 -c:a aac -t {duration_secs} \
             -f mpegts - | srt-live-transmit -q -chunk:1316 file://con \"{srt_url}\""
        );
        let child = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn pipeline");
        Self { child }
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        let pgid = self.child.id();
        kill_group(pgid);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Runs an arbitrary shell command in its own process group, killed on
/// drop. Same rationale as `Pipeline`: a receiving or listening
/// `srt-live-transmit` never exits on its own when its input or output
/// ends, so this kills the whole process group rather than waiting for it.
struct Guarded {
    child: Child,
}

impl Guarded {
    fn spawn(cmd: &str) -> Self {
        use std::os::unix::process::CommandExt;
        let child = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn guarded command");
        Self { child }
    }
}

impl Drop for Guarded {
    fn drop(&mut self) {
        let pgid = self.child.id();
        kill_group(pgid);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A unique path under the OS temp dir for one test's captured TS output.
fn ts_output_path(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("caudal_srt_{tag}_{}.ts", std::process::id()))
}

/// Runs `ffprobe -show_entries stream=codec_name,codec_type` on `path` and
/// returns `(stderr, stdout)`.
fn ffprobe_codecs(path: &std::path::Path) -> (String, String) {
    let output = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "stream=codec_name,codec_type", "-of", "json"])
        .arg(path)
        .output()
        .expect("run ffprobe");
    (String::from_utf8_lossy(&output.stderr).trim().to_owned(), String::from_utf8_lossy(&output.stdout).into_owned())
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

#[tokio::test(flavor = "multi_thread")]
async fn h264_aac_publish_end_to_end() {
    if !have("ffmpeg") || !have("srt-live-transmit") {
        eprintln!("SKIP: ffmpeg and/or srt-live-transmit not installed");
        return;
    }

    let server = TestServer::start(None).await;
    let url = format!("srt://127.0.0.1:{}?streamid=publish/test", server.port);
    let pipeline = Pipeline::start("1280x720", 30, &url);

    let stream = wait_for(Duration::from_secs(15), || server.registry.get("test"))
        .await
        .expect("stream 'test' never appeared in the registry");

    let tracks = wait_for_tracks(&stream, Duration::from_secs(8)).await;
    assert_eq!(tracks.len(), 2, "expected exactly one video and one audio track: {tracks:?}");

    let video = tracks.iter().find(|t| t.kind() == TrackKind::Video).expect("no video track");
    let audio = tracks.iter().find(|t| t.kind() == TrackKind::Audio).expect("no audio track");
    assert_eq!(video.codec, Codec::H264);
    assert_eq!(audio.codec, Codec::Aac);

    let vp = video.video.expect("video track missing VideoParams");
    assert_eq!((vp.width, vp.height), (1280, 720), "wrong dimensions parsed from SPS");

    let ap = audio.audio.expect("audio track missing AudioParams");
    assert_eq!(ap.sample_rate, 48_000, "wrong sample rate parsed from AudioSpecificConfig");

    // First video frame is a keyframe with valid AVCC framing; dts starts
    // near 0 and is monotonic per track.
    let mut sub = stream.subscribe(StartAt::Oldest);
    let mut saw_first_video = false;
    let mut last_video_dts: Option<i64> = None;
    let mut last_audio_dts: Option<i64> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), sub.recv()).await {
            Ok(Event::Frame(frame)) if frame.track == caudal_core::TrackId(0) => {
                if !saw_first_video {
                    assert!(frame.keyframe, "first video frame should be a keyframe");
                    assert!(looks_like_avcc(&frame.data), "video frame is not AVCC-prefixed");
                    assert!(frame.dts.abs() < 90_000, "first video dts not near 0: {}", frame.dts);
                    saw_first_video = true;
                }
                if let Some(prev) = last_video_dts {
                    assert!(frame.dts >= prev, "video dts went backwards: {prev} -> {}", frame.dts);
                }
                last_video_dts = Some(frame.dts);
            }
            Ok(Event::Frame(frame)) if frame.track == caudal_core::TrackId(1) => {
                if let Some(prev) = last_audio_dts {
                    assert!(frame.dts >= prev, "audio dts went backwards: {prev} -> {}", frame.dts);
                }
                last_audio_dts = Some(frame.dts);
            }
            _ => continue,
        }
        if saw_first_video && last_audio_dts.is_some() && stream.stats().frames_in > 60 {
            break;
        }
    }
    assert!(saw_first_video, "never observed a video frame");
    assert!(last_audio_dts.is_some(), "never observed an audio frame");

    wait_for(Duration::from_secs(4), || (stream.stats().frames_in > 60).then_some(()))
        .await
        .expect("frames_in never exceeded 60");

    // Mount the real HLS router on the same registry and confirm a real
    // player-facing playlist is produced, end to end.
    if have("ffprobe") {
        let hls_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hls_port = hls_listener.local_addr().unwrap().port();
        let router = caudal_hls::router(
            server.registry.clone(),
            caudal_hls::HlsConfig {
                part_ms: 200,
                segment_ms: 2000,
                cue_tags: true,
                cue_out_tags: false,
                ..caudal_hls::HlsConfig::default()
            },
        );
        tokio::spawn(async move {
            let _ = axum::serve(hls_listener, router).await;
        });
        // Give the packager a moment to produce an init segment.
        tokio::time::sleep(Duration::from_secs(2)).await;

        let master_url = format!("http://127.0.0.1:{hls_port}/hls/test/master.m3u8");
        let output = Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "stream=codec_name,codec_type", "-of", "json", &master_url])
            .output()
            .expect("run ffprobe");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.trim().is_empty(), "ffprobe reported errors on the HLS playlist: {stderr}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("h264"), "ffprobe did not see h264 in the HLS playlist: {stdout}");
        assert!(stdout.contains("aac"), "ffprobe did not see aac in the HLS playlist: {stdout}");
    } else {
        eprintln!("SKIP (partial): ffprobe not installed, HLS playback not verified");
    }

    // Hang up (srt-live-transmit never exits on its own; see `Pipeline`);
    // the stream must disappear within 5 s of the disconnect.
    drop(pipeline);
    let gone = wait_for(Duration::from_secs(5), || server.registry.get("test").is_none().then_some(())).await;
    assert!(gone.is_some(), "stream 'test' was not removed within 5s of the publisher disconnecting");
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_streamid_mode_is_rejected() {
    if !have("ffmpeg") || !have("srt-live-transmit") {
        eprintln!("SKIP: ffmpeg and/or srt-live-transmit not installed");
        return;
    }

    let server = TestServer::start(None).await;
    let url = format!("srt://127.0.0.1:{}?streamid=play/test", server.port);
    let pipeline = Pipeline::start("320x240", 3, &url);

    // Give the pipeline every chance to publish before asserting nothing
    // showed up: `play/...` is not a publish stream id and must be rejected.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(server.registry.get("test").is_none(), "a play/ stream id must never create a stream");
    assert!(server.registry.list().is_empty(), "a play/ stream id must never create any stream");

    drop(pipeline);
}

#[tokio::test(flavor = "multi_thread")]
async fn passphrase_required_and_enforced() {
    if !have("ffmpeg") || !have("srt-live-transmit") {
        eprintln!("SKIP: ffmpeg and/or srt-live-transmit not installed");
        return;
    }

    let passphrase = "correcthorsebattery"; // 19 bytes, within rsrt's 10..=80
    let server = TestServer::start(Some(passphrase)).await;

    // Correct passphrase: the stream comes up normally.
    let ok_url = format!("srt://127.0.0.1:{}?streamid=publish/withpass&passphrase={passphrase}", server.port);
    let ok_pipeline = Pipeline::start("320x240", 4, &ok_url);
    let stream = wait_for(Duration::from_secs(10), || server.registry.get("withpass")).await;
    assert!(stream.is_some(), "a caller with the correct passphrase must be able to publish");
    drop(ok_pipeline);

    // No passphrase at all: rsrt enforces encryption at the handshake, so
    // the connection never establishes and no stream ever appears.
    let missing_url = format!("srt://127.0.0.1:{}?streamid=publish/nopass", server.port);
    let missing_pipeline = Pipeline::start("320x240", 3, &missing_url);
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(server.registry.get("nopass").is_none(), "an unencrypted caller must be rejected when a passphrase is set");
    drop(missing_pipeline);
}

#[tokio::test(flavor = "multi_thread")]
async fn disconnect_ends_the_stream_within_5s() {
    if !have("ffmpeg") || !have("srt-live-transmit") {
        eprintln!("SKIP: ffmpeg and/or srt-live-transmit not installed");
        return;
    }

    let server = TestServer::start(None).await;
    let url = format!("srt://127.0.0.1:{}?streamid=publish/dc", server.port);
    let pipeline = Pipeline::start("320x240", 30, &url);

    wait_for(Duration::from_secs(10), || server.registry.get("dc")).await.expect("stream 'dc' never appeared");

    // Kill the pipeline outright (not a clean `-t` exit) and confirm the
    // stream is gone within 5s.
    drop(pipeline);
    let gone = wait_for(Duration::from_secs(5), || server.registry.get("dc").is_none().then_some(())).await;
    assert!(gone.is_some(), "stream 'dc' was not removed within 5s of a hard disconnect");
}

/// Pull: `play/<name>` serves a live stream as MPEG-TS. Publishes with the
/// same ffmpeg | srt-live-transmit pipeline as the ingest tests, then pulls
/// it back with a second, guarded `srt-live-transmit` acting as the viewer.
#[tokio::test(flavor = "multi_thread")]
async fn pull_serves_mpegts_to_a_viewer() {
    if !have("ffmpeg") || !have("srt-live-transmit") || !have("ffprobe") {
        eprintln!("SKIP: ffmpeg/srt-live-transmit/ffprobe not installed");
        return;
    }

    let server = TestServer::start(None).await;
    let publish_url = format!("srt://127.0.0.1:{}?streamid=publish/pull1", server.port);
    let publish_pipeline = Pipeline::start("640x360", 30, &publish_url);
    wait_for(Duration::from_secs(10), || server.registry.get("pull1")).await.expect("publish never appeared");
    let stream = server.registry.get("pull1").unwrap();
    wait_for_tracks(&stream, Duration::from_secs(8)).await;

    let out_path = ts_output_path("pull");
    let play_url = format!("srt://127.0.0.1:{}?streamid=play/pull1", server.port);
    let receiver = Guarded::spawn(&format!("srt-live-transmit -q \"{play_url}\" file://con > {}", out_path.display()));
    tokio::time::sleep(Duration::from_secs(5)).await;
    // `srt-live-transmit` never propagates its stdin EOF as a clean SRT
    // close (see the module doc): both ends here are always hard-killed,
    // which can truncate the very last video access unit mid-frame. That
    // is a property of the test's fixed-duration capture, not of the
    // muxer, so the decode check below only requires ffmpeg to succeed,
    // not a silent stderr.
    drop(receiver);
    drop(publish_pipeline);

    let len = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
    assert!(len > 0, "no TS bytes were received by the viewer");

    let (stderr, stdout) = ffprobe_codecs(&out_path);
    assert!(stderr.is_empty(), "ffprobe reported errors on the pulled output: {stderr}");
    assert!(stdout.contains("h264"), "no h264 in the pulled output: {stdout}");
    assert!(stdout.contains("aac"), "no aac in the pulled output: {stdout}");

    let decode = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&out_path)
        .args(["-f", "null", "-"])
        .output()
        .expect("run ffmpeg decode");
    let decode_err = String::from_utf8_lossy(&decode.stderr);
    assert!(decode.status.success(), "ffmpeg failed to decode the pulled output: {decode_err}");

    let _ = std::fs::remove_file(&out_path);
}

/// Pull of a stream name nobody is publishing: the connection closes with
/// no data, never creating a stream or hanging.
#[tokio::test(flavor = "multi_thread")]
async fn pull_of_unknown_stream_closes() {
    if !have("srt-live-transmit") {
        eprintln!("SKIP: srt-live-transmit not installed");
        return;
    }

    let server = TestServer::start(None).await;
    let out_path = ts_output_path("unknown");
    let play_url = format!("srt://127.0.0.1:{}?streamid=play/doesnotexist", server.port);
    let receiver = Guarded::spawn(&format!("srt-live-transmit -q \"{play_url}\" file://con > {}", out_path.display()));
    tokio::time::sleep(Duration::from_secs(2)).await;
    drop(receiver);

    let len = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
    assert_eq!(len, 0, "an unknown stream must never send any TS bytes");
    assert!(server.registry.list().is_empty(), "a play attempt must never create a stream");

    let _ = std::fs::remove_file(&out_path);
}

/// Play refused by an installed [`Gate`]: even though the stream exists and
/// is live, the connection closes with no data.
#[tokio::test(flavor = "multi_thread")]
async fn play_refused_by_gate_closes() {
    if !have("ffmpeg") || !have("srt-live-transmit") {
        eprintln!("SKIP: ffmpeg/srt-live-transmit not installed");
        return;
    }

    let server = TestServer::start_with_gate(Arc::new(DenyPlayGate)).await;
    let publish_url = format!("srt://127.0.0.1:{}?streamid=publish/gated", server.port);
    let publish_pipeline = Pipeline::start("320x240", 20, &publish_url);
    wait_for(Duration::from_secs(10), || server.registry.get("gated")).await.expect("publish never appeared");

    let out_path = ts_output_path("gated");
    let play_url = format!("srt://127.0.0.1:{}?streamid=play/gated", server.port);
    let receiver = Guarded::spawn(&format!("srt-live-transmit -q \"{play_url}\" file://con > {}", out_path.display()));
    tokio::time::sleep(Duration::from_secs(2)).await;
    drop(receiver);
    drop(publish_pipeline);

    let len = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
    assert_eq!(len, 0, "a gate-refused play must never send any TS bytes");

    let _ = std::fs::remove_file(&out_path);
}

/// Push: a `[[srt.push]]` entry connects out to a remote SRT listener and
/// sends the configured stream as MPEG-TS once it is live.
#[tokio::test(flavor = "multi_thread")]
async fn push_sends_a_live_stream_to_a_remote_listener() {
    if !have("ffmpeg") || !have("srt-live-transmit") || !have("ffprobe") {
        eprintln!("SKIP: ffmpeg/srt-live-transmit/ffprobe not installed");
        return;
    }

    let push_port = free_port();
    let out_path = ts_output_path("push");
    let listener =
        Guarded::spawn(&format!("srt-live-transmit -q \"srt://:{push_port}\" file://con > {}", out_path.display()));
    // Give the listener a moment to bind before Caudal's push tries to connect.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let push = SrtPush { stream: "pushed".to_owned(), url: format!("srt://127.0.0.1:{push_port}") };
    let server = TestServer::start_with_pushes(vec![push]).await;

    let publish_url = format!("srt://127.0.0.1:{}?streamid=publish/pushed", server.port);
    let publish_pipeline = Pipeline::start("640x360", 30, &publish_url);
    wait_for(Duration::from_secs(10), || server.registry.get("pushed")).await.expect("publish never appeared");

    tokio::time::sleep(Duration::from_secs(6)).await;
    drop(listener);
    drop(publish_pipeline);

    let len = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
    assert!(len > 0, "no TS bytes were pushed to the remote listener");

    let (stderr, stdout) = ffprobe_codecs(&out_path);
    assert!(stderr.is_empty(), "ffprobe reported errors on the pushed output: {stderr}");
    assert!(stdout.contains("h264"), "no h264 in the pushed output: {stdout}");
    assert!(stdout.contains("aac"), "no aac in the pushed output: {stdout}");

    let _ = std::fs::remove_file(&out_path);
}

/// SIGKILL to a whole process group. Never via `/bin/kill -KILL -<pgid>`:
/// Linux procps reads that as "every process of this user" (it killed the
/// GitHub runner mid-test).
fn kill_group(pgid: u32) {
    if let Some(pid) = rustix::process::Pid::from_raw(pgid as i32) {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
}
