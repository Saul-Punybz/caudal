//! `[[hls.pull]]` against real HLS: ffmpeg writing a live (or finished)
//! HLS into a temporary directory, served by a small HTTP server in the
//! test.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use caudal_core::{BufferConfig, Codec, Event, Registry, StartAt, TrackKind};
use caudal_hls_pull::{HlsPull, start_pulls};

fn have(bin: &str) -> bool {
    Command::new(bin).arg("-version").output().is_ok_and(|o| o.status.success())
}

/// Serves the files of `dir` over HTTP, as a CDN or origin would.
async fn serve(dir: PathBuf) -> SocketAddr {
    async fn file(State(dir): State<Arc<PathBuf>>, UrlPath(path): UrlPath<String>) -> Result<Vec<u8>, StatusCode> {
        if path.split('/').any(|p| p == "..") {
            return Err(StatusCode::BAD_REQUEST);
        }
        tokio::fs::read(dir.join(path)).await.map_err(|_| StatusCode::NOT_FOUND)
    }
    let app = axum::Router::new().route("/{*path}", axum::routing::get(file)).with_state(Arc::new(dir));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

/// ffmpeg writing HLS with MPEG-TS segments; killed on drop.
struct Encoder(Child);

impl Encoder {
    fn start(playlist: &Path, codecs: &[&str], size_rate: &str, live: bool, secs: u32) -> Self {
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-hide_banner", "-loglevel", "error"]);
        if live {
            cmd.arg("-re");
        }
        let child = cmd
            .args(["-f", "lavfi", "-i"])
            .arg(format!("testsrc2=size={size_rate}"))
            .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"])
            .args(codecs)
            .args(["-t", &secs.to_string(), "-f", "hls", "-hls_time", "1", "-hls_list_size"])
            .arg(if live { "5" } else { "0" })
            .arg(playlist)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ffmpeg");
        Self(child)
    }

    /// Runs a finished (VOD) HLS to completion.
    fn finished(playlist: &Path, secs: u32) {
        let mut e = Self::start(playlist, H264, "320x240:rate=25", false, secs);
        assert!(e.0.wait().unwrap().success(), "ffmpeg wrote {}", playlist.display());
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

const H264: &[&str] = &["-c:v", "libx264", "-preset", "veryfast", "-g", "25", "-c:a", "aac"];

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

fn codecs(s: &caudal_core::Stream) -> Vec<Codec> {
    s.tracks().iter().map(|t| t.codec).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_hls_publishes_and_ends_with_its_source() {
    if !have("ffmpeg") {
        eprintln!("SKIP: ffmpeg not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let addr = serve(dir.path().to_owned()).await;
    let registry = Registry::new();
    // The pull starts first: no playlist yet, nothing published.
    let pulls = start_pulls(
        registry.clone(),
        BufferConfig::default(),
        vec![HlsPull { stream: "tv".into(), url: format!("http://{addr}/live/index.m3u8") }],
    );
    std::fs::create_dir(dir.path().join("live")).unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(registry.get("tv").is_none(), "nothing published before segments");
    let encoder = Encoder::start(&dir.path().join("live/index.m3u8"), H264, "320x240:rate=25", true, 60);

    let stream = wait_for(Duration::from_secs(30), || registry.get("tv")).await.expect("published");
    let ok = wait_for(Duration::from_secs(10), || {
        let c = codecs(&stream);
        (c.contains(&Codec::H264) && c.contains(&Codec::Aac)).then_some(())
    })
    .await;
    assert!(ok.is_some(), "H.264 + AAC: {:?}", codecs(&stream));
    // Past the first segments: new ones keep being fetched as ffmpeg
    // writes them (25 fps video plus audio: well over 50 frames in 4 s).
    tokio::time::sleep(Duration::from_secs(3)).await;
    let before = stream.stats().frames_in;
    assert!(
        wait_for(Duration::from_secs(6), || (stream.stats().frames_in > before + 75).then_some(())).await.is_some(),
        "frames keep arriving: {} then {}",
        before,
        stream.stats().frames_in
    );

    // ffmpeg is killed (no EXT-X-ENDLIST): the playlist stops moving and
    // the stream ends.
    drop(encoder);
    let ended = wait_for(caudal_hls_pull::STALL_MIN + Duration::from_secs(6), || stream.is_ended().then_some(())).await;
    assert!(ended.is_some(), "the stream ended with its source");
    pulls.reload(&registry, Vec::new());
}

/// A master playlist: the highest-bandwidth variant is played (the other
/// one does not exist), and a broadcast feed (MPEG-2 + MP2) comes through.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn master_playlist_picks_the_best_variant_mpeg2_mp2() {
    if !have("ffmpeg") {
        eprintln!("SKIP: ffmpeg not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("hi")).unwrap();
    std::fs::write(
        dir.path().join("master.m3u8"),
        "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=500000\nmissing/index.m3u8\n\
         #EXT-X-STREAM-INF:BANDWIDTH=6000000,RESOLUTION=640x360\nhi/index.m3u8\n",
    )
    .unwrap();
    let _encoder = Encoder::start(
        &dir.path().join("hi/index.m3u8"),
        &["-c:v", "mpeg2video", "-b:v", "4M", "-g", "15", "-c:a", "mp2"],
        "640x360:rate=30000/1001",
        true,
        60,
    );
    let addr = serve(dir.path().to_owned()).await;
    let registry = Registry::new();
    let _pulls = start_pulls(
        registry.clone(),
        BufferConfig::default(),
        vec![HlsPull { stream: "catv".into(), url: format!("http://{addr}/master.m3u8") }],
    );
    let stream = wait_for(Duration::from_secs(30), || registry.get("catv")).await.expect("published");
    let ok = wait_for(Duration::from_secs(10), || {
        let c = codecs(&stream);
        (c.contains(&Codec::Mpeg2Video) && c.contains(&Codec::Mp2)).then_some(())
    })
    .await;
    assert!(ok.is_some(), "MPEG-2 + MP2: {:?}", codecs(&stream));
}

/// A finished playlist (`EXT-X-ENDLIST`) with an `EXT-X-DISCONTINUITY`
/// between two encodes whose clocks both start at the same time: played
/// out at its own pace on one monotonic timeline, then the stream ends and
/// is not replayed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn endlist_with_a_discontinuity() {
    if !have("ffmpeg") {
        eprintln!("SKIP: ffmpeg not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    for side in ["a", "b"] {
        std::fs::create_dir(dir.path().join(side)).unwrap();
        Encoder::finished(&dir.path().join(format!("{side}/index.m3u8")), 3);
    }
    // a's segments, a discontinuity, b's segments, the end.
    let mut joined = String::from("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:0\n");
    for side in ["a", "b"] {
        if side == "b" {
            joined.push_str("#EXT-X-DISCONTINUITY\n");
        }
        let text = std::fs::read_to_string(dir.path().join(format!("{side}/index.m3u8"))).unwrap();
        let mut lines = text.lines();
        while let Some(l) = lines.next() {
            if l.starts_with("#EXTINF") {
                joined.push_str(&format!("{l}\n{side}/{}\n", lines.next().unwrap()));
            }
        }
    }
    joined.push_str("#EXT-X-ENDLIST\n");
    std::fs::write(dir.path().join("joined.m3u8"), &joined).unwrap();

    let addr = serve(dir.path().to_owned()).await;
    let registry = Registry::new();
    let started = std::time::Instant::now();
    let _pulls = start_pulls(
        registry.clone(),
        BufferConfig::default(),
        vec![HlsPull { stream: "vod".into(), url: format!("http://{addr}/joined.m3u8") }],
    );
    let stream = wait_for(Duration::from_secs(20), || registry.get("vod")).await.expect("published");
    let mut viewer = stream.subscribe(StartAt::Oldest);
    let mut video_dts = Vec::new();
    let mut video_track = None;
    let collected = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match viewer.recv().await {
                Event::TracksChanged => {
                    video_track = viewer.tracks().into_iter().find(|t| t.kind() == TrackKind::Video).map(|t| t.id);
                }
                Event::Frame(f) if Some(f.track) == video_track => video_dts.push(f.dts),
                Event::End => break,
                _ => {}
            }
        }
    })
    .await;
    assert!(collected.is_ok(), "the stream ended after EXT-X-ENDLIST");
    assert!(stream.is_ended());
    // 6 s of 1 s segments, fed at most two target durations ahead: the
    // last one goes in no sooner than 3 s after the first.
    assert!(started.elapsed() >= Duration::from_millis(2900), "paced, not dumped at once: {:?}", started.elapsed());
    assert!(video_dts.len() > 100, "both encodes' video (150 frames): {}", video_dts.len());
    assert!(video_dts.windows(2).all(|w| w[1] > w[0]), "one monotonic timeline across the discontinuity");
    let span = (video_dts.last().unwrap() - video_dts[0]) as f64 / 90_000.0;
    assert!((5.0..6.5).contains(&span), "6 s of media end to end: {span} s");
    // Not replayed: the playlist still ends with EXT-X-ENDLIST.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(registry.get("vod").is_none_or(|s| Arc::ptr_eq(&s, &stream)), "not published again");
}
