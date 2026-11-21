//! `[[rtmp.pull]]` against a real RTMP server: ffmpeg in listen mode
//! (`-f flv -listen 1 rtmp://...`) serves one player, which is Caudal's
//! pull (handshake, `connect`, `createStream`, `play`, `onStatus`, media).
//! Skipped (with a "SKIP" line) when ffmpeg is missing.

use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use caudal_core::{BufferConfig, Codec, Registry};
use caudal_rtmp::{RtmpPull, start_pulls};

fn have(bin: &str) -> bool {
    Command::new(bin).arg("-version").output().is_ok_and(|o| o.status.success())
}

fn have_libx265() -> bool {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("libx265"))
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// ffmpeg serving one RTMP player at `url`; killed on drop (one process).
struct Source(Child);

impl Source {
    fn start(url: &str, codecs: &[&str]) -> Self {
        let child = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-re", "-f", "lavfi", "-i"])
            .arg("testsrc2=size=320x240:rate=25")
            .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"])
            .args(codecs)
            .args(["-t", "60", "-f", "flv", "-listen", "1", url])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ffmpeg");
        Self(child)
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
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

const H264_AAC: [&str; 8] = ["-c:v", "libx264", "-preset", "veryfast", "-g", "25", "-c:a", "aac"];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plays_from_an_rtmp_server_ends_and_reconnects() {
    if !have("ffmpeg") {
        eprintln!("SKIP: ffmpeg not installed");
        return;
    }
    let port = free_port();
    let url = format!("rtmp://127.0.0.1:{port}/live/cam1");
    let registry = Registry::new();
    // The pull starts first: it retries until the server listens, and
    // publishes nothing meanwhile.
    let pulls = start_pulls(
        registry.clone(),
        BufferConfig::default(),
        vec![RtmpPull { stream: "remote".into(), url: url.clone() }],
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(registry.get("remote").is_none(), "nothing published before media");
    let source = Source::start(&url, &H264_AAC);

    let stream = wait_for(Duration::from_secs(30), || registry.get("remote")).await.expect("published");
    let codecs = wait_for(Duration::from_secs(10), || {
        let c: Vec<Codec> = stream.tracks().iter().map(|t| t.codec).collect();
        (c.contains(&Codec::H264) && c.contains(&Codec::Aac)).then_some(c)
    })
    .await;
    assert!(codecs.is_some(), "H.264 + AAC: {:?}", stream.tracks().iter().map(|t| t.codec).collect::<Vec<_>>());
    let video = stream.tracks().into_iter().find(|t| t.codec == Codec::H264).unwrap();
    assert_eq!(video.video.map(|v| (v.width, v.height)), Some((320, 240)), "avcC dimensions");
    let before = stream.stats().frames_in;
    assert!(
        wait_for(Duration::from_secs(5), || (stream.stats().frames_in > before + 25).then_some(())).await.is_some(),
        "frames keep arriving"
    );

    // The server goes away: the stream ends.
    drop(source);
    let ended = wait_for(caudal_rtmp::pull::IDLE + Duration::from_secs(3), || stream.is_ended().then_some(())).await;
    assert!(ended.is_some(), "the stream ended with its source");

    // It comes back: the pull reconnects and publishes the name anew.
    let _source = Source::start(&url, &H264_AAC);
    let again = wait_for(Duration::from_secs(40), || registry.get("remote").filter(|s| !s.is_ended())).await;
    let again = again.expect("republished");
    assert!(!Arc::ptr_eq(&again, &stream), "a new publish");
    let n = wait_for(Duration::from_secs(10), || (again.stats().frames_in > 25).then_some(())).await;
    assert!(n.is_some(), "frames after the reconnect");

    // A reload without the pull stops it and ends the stream.
    pulls.reload(&registry, Vec::new());
    let stopped = wait_for(Duration::from_secs(3), || again.is_ended().then_some(())).await;
    assert!(stopped.is_some(), "removing the pull ends its stream");
}

/// Enhanced RTMP HEVC (ffmpeg's FLV muxer writes `hvc1` FourCC tags)
/// goes through the same demuxer as a publisher's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plays_enhanced_rtmp_hevc() {
    if !have("ffmpeg") || !have_libx265() {
        eprintln!("SKIP: ffmpeg with libx265 not installed");
        return;
    }
    let port = free_port();
    let url = format!("rtmp://127.0.0.1:{port}/live/hevc");
    let registry = Registry::new();
    let _source = Source::start(
        &url,
        &["-c:v", "libx265", "-preset", "ultrafast", "-x265-params", "log-level=error", "-g", "25", "-c:a", "aac"],
    );
    let _pulls = start_pulls(registry.clone(), BufferConfig::default(), vec![RtmpPull { stream: "h".into(), url }]);
    let stream = wait_for(Duration::from_secs(30), || registry.get("h")).await.expect("published");
    let ok = wait_for(Duration::from_secs(10), || {
        let c: Vec<Codec> = stream.tracks().iter().map(|t| t.codec).collect();
        (c.contains(&Codec::H265) && c.contains(&Codec::Aac)).then_some(())
    })
    .await;
    assert!(ok.is_some(), "H.265 + AAC: {:?}", stream.tracks().iter().map(|t| t.codec).collect::<Vec<_>>());
}
