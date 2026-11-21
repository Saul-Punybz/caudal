//! RTMP play, end to end with real tools: `ffmpeg` publishes into a real
//! `caudal_rtmp::serve` listener, a second `ffmpeg` plays the stream back
//! over RTMP into an FLV file, and `ffprobe` checks what came out.
//!
//! Every test checks for `ffmpeg`/`ffprobe` first and prints "SKIP" instead
//! of failing when they are missing, per the project's testing rules.

use std::net::IpAddr;
use std::net::TcpListener as StdTcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;

use caudal_core::{Access, BufferConfig, Denied, Gate, GateFuture, KickResult, Registry};
use caudal_rtmp::{RtmpConfig, serve};

fn have(tool: &str) -> bool {
    Command::new(tool).arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

fn have_tools() -> bool {
    have("ffmpeg") && have("ffprobe")
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

struct TestServer {
    registry: Arc<Registry>,
    port: u16,
    handle: tokio::task::JoinHandle<()>,
}

impl TestServer {
    async fn start(play: bool) -> Self {
        let registry = Registry::new();
        for _ in 0..5 {
            let port = free_port();
            let cfg = RtmpConfig {
                bind: format!("127.0.0.1:{port}").parse().unwrap(),
                app: "live".into(),
                buffer: BufferConfig::default(),
                play,
            };
            let reg = registry.clone();
            let handle = tokio::spawn(async move {
                let _ = serve(cfg, reg).await;
            });
            let up =
                wait_for(Duration::from_secs(2), || std::net::TcpStream::connect(("127.0.0.1", port)).ok().map(|_| ()))
                    .await;
            if up.is_some() && !handle.is_finished() {
                return Self { registry, port, handle };
            }
            handle.abort();
        }
        panic!("rtmp listener could not bind a free port in 5 tries");
    }

    fn url(&self, name: &str) -> String {
        format!("rtmp://127.0.0.1:{}/live/{name}", self.port)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

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

async fn wait_for_exit(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    wait_for(timeout, || child.try_wait().ok().flatten()).await
}

/// Kills the child when dropped, so a failing assert never leaks ffmpeg.
struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn ffmpeg(args: &[&str]) -> Proc {
    Proc(
        Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ffmpeg"),
    )
}

/// A live H.264 (or HEVC) + AAC publish of `secs` seconds into `url`.
fn publish(url: &str, hevc: bool, secs: u32) -> Proc {
    let secs = secs.to_string();
    ffmpeg(&[
        "-re",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=640x360:rate=30",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=48000",
        "-c:v",
        if hevc { "libx265" } else { "libx264" },
        "-preset",
        "ultrafast",
        "-g",
        "30",
        "-c:a",
        "aac",
        "-t",
        &secs,
        "-f",
        "flv",
        url,
    ])
}

/// Plays `url` for at most `secs` seconds into `out` (stream copy).
fn play(url: &str, secs: u32, out: &std::path::Path) -> Proc {
    let secs = secs.to_string();
    ffmpeg(&["-i", url, "-t", &secs, "-c", "copy", "-f", "flv", out.to_str().unwrap()])
}

fn out_file(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("caudal-rtmp-play-{}-{name}.flv", std::process::id()))
}

/// `ffprobe` on `path`: `(codec_type, codec_name, frames)` per stream.
fn probe(path: &std::path::Path) -> Vec<(String, String, u64)> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-count_packets",
            "-show_entries",
            "stream=codec_type,codec_name,nb_read_packets",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("run ffprobe");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut f = l.split(',');
            let name = f.next()?.to_owned();
            let kind = f.next()?.to_owned();
            let n = f.next()?.trim().parse().ok()?;
            Some((kind, name, n))
        })
        .collect()
}

async fn wait_live(server: &TestServer, name: &str) {
    wait_for(Duration::from_secs(10), || {
        server.registry.get(name).filter(|s| s.tracks().len() == 2 && s.stats().frames_in > 30).map(|_| ())
    })
    .await
    .expect("the published stream never went live with two tracks");
}

async fn play_back_and_probe(hevc: bool) {
    let server = TestServer::start(true).await;
    let name = if hevc { "hevc" } else { "cam" };
    let _publisher = publish(&server.url(name), hevc, 20);
    wait_live(&server, name).await;

    let out = out_file(name);
    let mut viewer = play(&server.url(name), 5, &out);
    // The viewer shows up as a session while it plays.
    let session = wait_for(Duration::from_secs(5), || server.registry.sessions().list(Some(name)).into_iter().next())
        .await
        .expect("no rtmp viewer session");
    assert_eq!(session.protocol, "rtmp");
    assert_eq!(session.ip, Some(IpAddr::from([127, 0, 0, 1])));

    let status = wait_for_exit(&mut viewer.0, Duration::from_secs(15)).await.expect("ffmpeg play never finished");
    assert!(status.success(), "ffmpeg play failed: {status}");

    let streams = probe(&out);
    let _ = std::fs::remove_file(&out);
    let video = streams.iter().find(|s| s.0 == "video").unwrap_or_else(|| panic!("no video in {streams:?}"));
    let audio = streams.iter().find(|s| s.0 == "audio").unwrap_or_else(|| panic!("no audio in {streams:?}"));
    assert_eq!(video.1, if hevc { "hevc" } else { "h264" });
    assert_eq!(audio.1, "aac");
    // 5 s at 30 fps and 48 kHz AAC (~47 frames/s); generous lower bounds.
    assert!(video.2 >= 100, "too few video frames: {streams:?}");
    assert!(audio.2 >= 150, "too few audio frames: {streams:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn h264_aac_plays_back() {
    if !have_tools() {
        eprintln!("SKIP: ffmpeg/ffprobe not installed");
        return;
    }
    play_back_and_probe(false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn hevc_plays_back_over_enhanced_rtmp() {
    if !have_tools() || !have_libx265() {
        eprintln!("SKIP: ffmpeg without libx265");
        return;
    }
    play_back_and_probe(true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn play_decodes_cleanly() {
    if !have_tools() {
        eprintln!("SKIP: ffmpeg/ffprobe not installed");
        return;
    }
    let server = TestServer::start(true).await;
    let _publisher = publish(&server.url("dec"), false, 20);
    wait_live(&server, "dec").await;

    // Decode (not copy) 3 s straight from RTMP: any missing parameter set or
    // a first frame that is not a keyframe shows up as decoder errors.
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-v", "error", "-nostdin", "-i", &server.url("dec"), "-t", "3", "-f", "null", "-"])
        .output()
        .expect("run ffmpeg");
    assert!(out.status.success(), "ffmpeg decode failed: {}", String::from_utf8_lossy(&out.stderr));
    let errors = String::from_utf8_lossy(&out.stderr);
    assert!(errors.trim().is_empty(), "decoder errors: {errors}");
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_stream_fails_cleanly() {
    if !have_tools() {
        eprintln!("SKIP: ffmpeg/ffprobe not installed");
        return;
    }
    let server = TestServer::start(true).await;
    let out = out_file("nobody");
    let mut viewer = play(&server.url("nobody"), 3, &out);
    let status = wait_for_exit(&mut viewer.0, Duration::from_secs(5)).await.expect("ffmpeg kept waiting");
    assert!(!status.success());
    assert!(server.registry.sessions().list(None).is_empty());
    let _ = std::fs::remove_file(&out);
}

#[tokio::test(flavor = "multi_thread")]
async fn play_can_be_switched_off() {
    if !have_tools() {
        eprintln!("SKIP: ffmpeg/ffprobe not installed");
        return;
    }
    let server = TestServer::start(false).await;
    let _publisher = publish(&server.url("off"), false, 20);
    wait_live(&server, "off").await;
    let out = out_file("off");
    let mut viewer = play(&server.url("off"), 3, &out);
    let status = wait_for_exit(&mut viewer.0, Duration::from_secs(5)).await.expect("ffmpeg kept waiting");
    assert!(!status.success());
    let _ = std::fs::remove_file(&out);
}

/// Requires `?token=letmein` to play; publishing is open.
struct TokenGate;

impl Gate for TokenGate {
    fn check<'a>(
        &'a self,
        access: Access,
        _stream: &'a str,
        token: Option<&'a str>,
        _ip: Option<IpAddr>,
    ) -> GateFuture<'a> {
        Box::pin(async move {
            match (access, token) {
                (Access::Publish, _) => Ok(()),
                (Access::Play, Some("letmein")) => Ok(()),
                (Access::Play, Some(_)) => Err(Denied::Refused("bad token".into())),
                (Access::Play, None) => Err(Denied::Missing),
            }
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn play_needs_the_token_when_the_gate_asks() {
    if !have_tools() {
        eprintln!("SKIP: ffmpeg/ffprobe not installed");
        return;
    }
    let server = TestServer::start(true).await;
    server.registry.set_gate(Arc::new(TokenGate));
    let _publisher = publish(&server.url("auth"), false, 20);
    wait_live(&server, "auth").await;

    for bad in ["auth", "auth?token=wrong"] {
        let out = out_file("auth-bad");
        let mut viewer = play(&server.url(bad), 2, &out);
        let status = wait_for_exit(&mut viewer.0, Duration::from_secs(5)).await.expect("ffmpeg kept waiting");
        assert!(!status.success(), "{bad} should be refused");
        let _ = std::fs::remove_file(&out);
    }
    assert!(server.registry.sessions().list(None).is_empty());

    let out = out_file("auth-ok");
    let mut viewer = play(&server.url("auth?token=letmein"), 2, &out);
    let status = wait_for_exit(&mut viewer.0, Duration::from_secs(10)).await.expect("ffmpeg play never finished");
    assert!(status.success(), "the right token should play");
    let streams = probe(&out);
    let _ = std::fs::remove_file(&out);
    assert!(streams.iter().any(|s| s.0 == "video" && s.2 > 0), "nothing played: {streams:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn kicked_viewer_is_disconnected() {
    if !have_tools() {
        eprintln!("SKIP: ffmpeg/ffprobe not installed");
        return;
    }
    let server = TestServer::start(true).await;
    let _publisher = publish(&server.url("kick"), false, 30);
    wait_live(&server, "kick").await;

    let out = out_file("kick");
    // Would play for 20 s if nobody stopped it.
    let mut viewer = play(&server.url("kick"), 20, &out);
    let session = wait_for(Duration::from_secs(5), || server.registry.sessions().list(Some("kick")).into_iter().next())
        .await
        .expect("no rtmp viewer session");
    wait_for(Duration::from_secs(5), || {
        server.registry.sessions().list(Some("kick")).first().is_some_and(|s| s.bytes_out > 10_000).then_some(())
    })
    .await
    .expect("bytes_out never counted");

    assert_eq!(server.registry.sessions().kick(session.id, None), KickResult::Kicked { blocked: false });
    let exited = wait_for_exit(&mut viewer.0, Duration::from_secs(5)).await;
    assert!(exited.is_some(), "kicked viewer's connection stayed open");
    wait_for(Duration::from_secs(2), || server.registry.sessions().list(None).is_empty().then_some(()))
        .await
        .expect("session not closed after the kick");
    let _ = std::fs::remove_file(&out);
}

#[tokio::test(flavor = "multi_thread")]
async fn viewer_ends_when_the_stream_ends() {
    if !have_tools() {
        eprintln!("SKIP: ffmpeg/ffprobe not installed");
        return;
    }
    let server = TestServer::start(true).await;
    let mut publisher = publish(&server.url("short"), false, 20);
    wait_live(&server, "short").await;

    let out = out_file("short");
    let mut viewer = play(&server.url("short"), 30, &out);
    wait_for(Duration::from_secs(5), || server.registry.sessions().list(Some("short")).first().map(|_| ()))
        .await
        .expect("no rtmp viewer session");
    tokio::time::sleep(Duration::from_secs(1)).await;
    let _ = publisher.0.kill();
    let _ = publisher.0.wait();

    // StreamEOF + NetStream.Play.Stop, then the connection closes: ffmpeg
    // finishes the file and exits well before its 30 s.
    let status = wait_for_exit(&mut viewer.0, Duration::from_secs(10)).await.expect("viewer kept waiting");
    assert!(status.success(), "ffmpeg should end cleanly at the stream's end: {status}");
    let streams = probe(&out);
    let _ = std::fs::remove_file(&out);
    assert!(streams.iter().any(|s| s.0 == "video" && s.2 > 0), "nothing played: {streams:?}");
    assert!(server.registry.sessions().list(None).is_empty());
}

/// A viewer that stops reading (SIGSTOP) must not make the server buffer
/// for it: its write times out and the session is dropped. Unix-only
/// process control.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn stalled_viewer_is_dropped() {
    if !have_tools() {
        eprintln!("SKIP: ffmpeg/ffprobe not installed");
        return;
    }
    let server = TestServer::start(true).await;
    // A high bitrate fills the socket buffers quickly.
    let url = server.url("stall");
    let _publisher = ffmpeg(&[
        "-re",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=1280x720:rate=30",
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-b:v",
        "20M",
        "-g",
        "30",
        "-t",
        "60",
        "-f",
        "flv",
        &url,
    ]);
    wait_for(Duration::from_secs(10), || server.registry.get("stall").filter(|s| s.stats().frames_in > 30).map(|_| ()))
        .await
        .expect("publish never went live");

    let out = out_file("stall");
    let viewer = play(&url, 60, &out);
    wait_for(Duration::from_secs(5), || server.registry.sessions().list(Some("stall")).first().map(|_| ()))
        .await
        .expect("no rtmp viewer session");
    let pid = viewer.0.id().to_string();
    assert!(Command::new("kill").args(["-STOP", &pid]).status().unwrap().success());

    let dropped =
        wait_for(Duration::from_secs(40), || server.registry.sessions().list(Some("stall")).is_empty().then_some(()))
            .await;
    let _ = Command::new("kill").args(["-CONT", &pid]).status();
    let _ = std::fs::remove_file(&out);
    assert!(dropped.is_some(), "a viewer that stopped reading was never dropped");
}
