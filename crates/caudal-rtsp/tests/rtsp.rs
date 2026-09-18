//! Integration tests: real `ffprobe`/`ffmpeg` against `caudal_rtsp::serve`'s
//! RTSP server (fed from the `caudal-hls` H.264+AAC fixture), and a real
//! `caudal_rtsp::run` pull that connects to that same server as if it were
//! a camera.

#[path = "../../caudal-hls/src/mp4demux.rs"]
mod mp4demux;

use std::net::TcpListener as StdTcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use caudal_core::{Access, BufferConfig, Codec, Denied, Event, Gate, GateFuture, Registry, StartAt, TrackKind};
use caudal_rtsp::{RtspConfig, RtspPull};
use mp4demux::Demuxed;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn have(bin: &str) -> bool {
    Command::new("which").arg(bin).output().is_ok_and(|o| o.status.success())
}

fn free_port() -> u16 {
    StdTcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn fixture() -> Demuxed {
    mp4demux::demux(include_bytes!("../../caudal-hls/tests/fixtures/av.mp4"))
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

/// A registry publishing the fixture (looped, at real time) as `name`,
/// alongside the RTSP server serving it on a free port. Dropping this stops
/// the feeder and ends the publish; the RTSP server task and its
/// connections keep running until `kill` is called (or the test process
/// exits), matching how a real camera / server pair behaves.
#[allow(dead_code)]
struct Source {
    pub registry: Arc<Registry>,
    pub port: u16,
    server: tokio::task::JoinHandle<()>,
    feeder: tokio::task::JoinHandle<()>,
    stop: Arc<AtomicBool>,
}

impl Source {
    async fn start(name: &str, port: u16, gate: Option<Arc<dyn Gate>>) -> Self {
        let fx = fixture();
        let registry = Registry::new();
        if let Some(gate) = gate {
            registry.set_gate(gate);
        }
        let publisher = registry.publish(name, BufferConfig::default()).unwrap();
        publisher.set_tracks(fx.tracks.clone()).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let feeder = {
            let stop = stop.clone();
            tokio::spawn(async move {
                let start = tokio::time::Instant::now();
                for n in 0.. {
                    for f in fx.looped(n) {
                        if stop.load(Ordering::Relaxed) {
                            drop(publisher);
                            return;
                        }
                        let at = start + Duration::from_micros(fx.micros(&f) as u64);
                        tokio::time::sleep_until(at).await;
                        if publisher.push(f).is_err() {
                            return;
                        }
                    }
                }
            })
        };

        let cfg = RtspConfig {
            bind: Some(format!("127.0.0.1:{port}").parse().unwrap()),
            pulls: Vec::new(),
            buffer: BufferConfig::default(),
        };
        let reg = registry.clone();
        let server = tokio::spawn(async move {
            let _ = caudal_rtsp::serve(cfg, reg).await;
        });
        // Let the listener bind before anyone tries to connect.
        tokio::time::sleep(Duration::from_millis(200)).await;
        Self { registry, port, server, feeder, stop }
    }

    /// Ends the publish and stops the RTSP server's accept loop. Already
    /// accepted connections are handled by their own detached tasks and are
    /// closed on the source side once their `Event::End` fires (see
    /// `server.rs`), not by this call.
    fn kill(self) {
        self.stop.store(true, Ordering::Relaxed);
        self.server.abort();
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.server.abort();
        self.feeder.abort();
    }
}

/// Runs a shell command in its own process group with a hard wall-clock
/// limit, killing the whole group on timeout or drop (never left to busy
/// loop or hang, per the batch's machine rules).
struct GuardedChild(Child);

impl Drop for GuardedChild {
    fn drop(&mut self) {
        let pgid = self.0.id();
        let _ = Command::new("kill").args(["-KILL", &format!("-{pgid}")]).status();
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn run_cmd(shell: &str, timeout: Duration) -> (bool, String, String) {
    use std::os::unix::process::CommandExt;
    let child = Command::new("sh")
        .arg("-c")
        .arg(shell)
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    let mut guard = GuardedChild(child);
    let start = std::time::Instant::now();
    loop {
        if let Some(status) = guard.0.try_wait().expect("try_wait") {
            let mut stdout = String::new();
            let mut stderr = String::new();
            if let Some(mut s) = guard.0.stdout.take() {
                let _ = std::io::Read::read_to_string(&mut s, &mut stdout);
            }
            if let Some(mut s) = guard.0.stderr.take() {
                let _ = std::io::Read::read_to_string(&mut s, &mut stderr);
            }
            return (status.success(), stdout, stderr);
        }
        if start.elapsed() > timeout {
            return (false, String::new(), "timed out".to_owned());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Sends a bare `DESCRIBE` over raw TCP and returns the status code, without
/// needing ffmpeg/ffprobe for the 404 / 401 / 403 cases.
async fn describe_status(port: u16, path: &str) -> u16 {
    let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
    let req = format!("DESCRIBE rtsp://127.0.0.1:{port}/{path} RTSP/1.0\r\nCSeq: 1\r\n\r\n");
    sock.write_all(req.as_bytes()).await.expect("write");
    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(Duration::from_secs(5), sock.read(&mut buf))
        .await
        .expect("response timed out")
        .expect("read");
    let text = String::from_utf8_lossy(&buf[..n]);
    text.split_whitespace().nth(1).expect("status line").parse().expect("status code")
}

struct DenyGate(Denied);

impl Gate for DenyGate {
    fn check<'a>(&'a self, access: Access, _stream: &'a str, _token: Option<&'a str>) -> GateFuture<'a> {
        let d = self.0.clone();
        Box::pin(async move {
            match access {
                Access::Play => Err(d),
                Access::Publish => Ok(()),
            }
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn describe_setup_play_over_ffmpeg() {
    if !have("ffprobe") || !have("ffmpeg") {
        eprintln!("SKIP: ffprobe/ffmpeg not installed");
        return;
    }
    let port = free_port();
    let source = Source::start("test", port, None).await;

    let url = format!("rtsp://127.0.0.1:{port}/test");
    let (ok, out, err) = run_cmd(
        &format!(
            "ffprobe -v error -rtsp_transport tcp -show_entries stream=codec_name -of csv=p=0 '{url}'"
        ),
        Duration::from_secs(20),
    );
    assert!(ok, "ffprobe failed: stdout={out} stderr={err}");
    assert!(out.contains("h264"), "ffprobe codecs did not include h264: {out}");
    assert!(out.contains("aac"), "ffprobe codecs did not include aac: {out}");

    let (ok, out, err) = run_cmd(
        &format!("ffmpeg -v error -rtsp_transport tcp -i '{url}' -t 3 -f null -"),
        Duration::from_secs(25),
    );
    assert!(ok, "ffmpeg decode failed: stdout={out} stderr={err}");
    assert!(err.trim().is_empty(), "ffmpeg reported errors: {err}");

    source.kill();
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_stream_is_404() {
    let port = free_port();
    let source = Source::start("test", port, None).await;
    let status = describe_status(port, "no-such-stream").await;
    assert_eq!(status, 404);
    source.kill();
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_token_is_401() {
    let port = free_port();
    let source = Source::start("test", port, Some(Arc::new(DenyGate(Denied::Missing)))).await;
    let status = describe_status(port, "test").await;
    assert_eq!(status, 401);
    source.kill();
}

#[tokio::test(flavor = "multi_thread")]
async fn refused_token_is_403() {
    let port = free_port();
    let source = Source::start("test", port, Some(Arc::new(DenyGate(Denied::Refused("nope".into()))))).await;
    let status = describe_status(port, "test").await;
    assert_eq!(status, 403);
    source.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pull_republishes_camera_and_reconnects() {
    let port = free_port();
    let source = Source::start("test", port, None).await;

    let dest = Registry::new();
    let pull_cfg = RtspConfig {
        bind: None,
        pulls: vec![RtspPull { stream: "cam".to_owned(), url: format!("rtsp://127.0.0.1:{port}/test") }],
        buffer: BufferConfig::default(),
    };
    let dest2 = dest.clone();
    let pull_handle = tokio::spawn(async move {
        let _ = caudal_rtsp::serve(pull_cfg, dest2).await;
    });

    let stream = wait_for(Duration::from_secs(15), || dest.get("cam")).await.expect("'cam' never appeared");
    let tracks = wait_for(Duration::from_secs(10), || {
        let t = stream.tracks();
        (!t.is_empty()).then_some(t)
    })
    .await
    .expect("tracks never announced");

    let video = tracks.iter().find(|t| t.kind() == TrackKind::Video).expect("no video track");
    let audio = tracks.iter().find(|t| t.kind() == TrackKind::Audio).expect("no audio track");
    assert_eq!(video.codec, Codec::H264);
    assert_eq!(audio.codec, Codec::Aac);
    let vp = video.video.expect("video track missing dimensions");
    assert_eq!((vp.width, vp.height), (256, 144), "wrong dimensions pulled from the fixture-fed source");

    // Frames actually flow.
    let mut sub = stream.subscribe(StartAt::Oldest);
    let got_frame = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Event::Frame(_) = sub.recv().await {
                return;
            }
        }
    })
    .await;
    assert!(got_frame.is_ok(), "no frames flowed from the pulled camera");
    drop(sub);

    // Reconnect: kill the source (server task aborted, publish ended), free
    // its port, then serve the same stream again on the same port. The pull
    // must notice (the server side closes the connection when its stream
    // ends, see server.rs) and come back on its own.
    source.kill();
    // Let the abort actually free the listening socket.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let source2 = Source::start("test", port, None).await;

    let mut sub = stream.subscribe(StartAt::LiveEdge);
    let reconnected = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Event::Frame(_) = sub.recv().await {
                return;
            }
        }
    })
    .await;
    assert!(reconnected.is_ok(), "pull never reconnected after the source restarted");

    pull_handle.abort();
    source2.kill();
}
