//! `[[http.pull]]` against a real source: ffmpeg serving MPEG-TS over HTTP
//! (`-listen 1`), the way VLC's `access=http,mux=ts` does.

use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use caudal_core::{BufferConfig, Codec, Registry};
use caudal_http_pull::{HttpPull, start_pulls};

fn have(bin: &str) -> bool {
    Command::new(bin).arg("-version").output().is_ok_and(|o| o.status.success())
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// ffmpeg serving one client at `url`; killed on drop (one process).
struct Source(Child);

impl Source {
    fn start(url: &str, codecs: &[&str], size_rate: &str) -> Self {
        let child = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-re", "-f", "lavfi", "-i"])
            .arg(format!("testsrc2=size={size_rate}"))
            .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"])
            .args(codecs)
            .args(["-t", "60", "-f", "mpegts", "-listen", "1", url])
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ts_over_http_publishes_ends_and_reconnects() {
    if !have("ffmpeg") {
        eprintln!("SKIP: ffmpeg not installed");
        return;
    }
    let port = free_port();
    let url = format!("http://127.0.0.1:{port}/stream.ts");
    let h264 = ["-c:v", "libx264", "-preset", "veryfast", "-g", "25", "-c:a", "aac"];
    let registry = Registry::new();
    // The pull starts first: it retries until the source listens, and
    // publishes nothing meanwhile.
    let pulls = start_pulls(
        registry.clone(),
        BufferConfig::default(),
        vec![HttpPull { stream: "web".into(), url: url.clone() }],
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(registry.get("web").is_none(), "nothing published before data");
    let source = Source::start(&url, &h264, "320x240:rate=25");

    let stream = wait_for(Duration::from_secs(20), || registry.get("web")).await.expect("published");
    let codecs = wait_for(Duration::from_secs(10), || {
        let c: Vec<Codec> = stream.tracks().iter().map(|t| t.codec).collect();
        (c.contains(&Codec::H264) && c.contains(&Codec::Aac)).then_some(c)
    })
    .await;
    assert!(codecs.is_some(), "H.264 + AAC: {:?}", stream.tracks().iter().map(|t| t.codec).collect::<Vec<_>>());
    let before = stream.stats().frames_in;
    assert!(
        wait_for(Duration::from_secs(5), || (stream.stats().frames_in > before + 25).then_some(())).await.is_some(),
        "frames keep arriving"
    );

    // The source goes away: the stream ends.
    drop(source);
    let ended = wait_for(caudal_http_pull::IDLE + Duration::from_secs(3), || stream.is_ended().then_some(())).await;
    assert!(ended.is_some(), "the stream ended with its source");

    // It comes back: the pull reconnects and publishes the name anew.
    let _source = Source::start(&url, &h264, "320x240:rate=25");
    let again = wait_for(Duration::from_secs(40), || registry.get("web").filter(|s| !s.is_ended())).await;
    assert!(again.is_some_and(|s| !Arc::ptr_eq(&s, &stream)), "republished");
    pulls.reload(&registry, Vec::new());
}

/// The same path carries a broadcast feed (MPEG-2 + MP2) as it is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mpeg2_and_mp2_over_http() {
    if !have("ffmpeg") {
        eprintln!("SKIP: ffmpeg not installed");
        return;
    }
    let port = free_port();
    let url = format!("http://127.0.0.1:{port}/");
    let registry = Registry::new();
    let _source = Source::start(
        &url,
        &["-c:v", "mpeg2video", "-b:v", "4M", "-g", "15", "-c:a", "mp2"],
        "640x360:rate=30000/1001",
    );
    let _pulls = start_pulls(registry.clone(), BufferConfig::default(), vec![HttpPull { stream: "catv".into(), url }]);
    let stream = wait_for(Duration::from_secs(20), || registry.get("catv")).await.expect("published");
    let ok = wait_for(Duration::from_secs(10), || {
        let t = stream.tracks();
        (t.iter().any(|t| t.codec == Codec::Mpeg2Video) && t.iter().any(|t| t.codec == Codec::Mp2)).then_some(())
    })
    .await;
    assert!(ok.is_some(), "MPEG-2 + MP2: {:?}", stream.tracks().iter().map(|t| t.codec).collect::<Vec<_>>());
}
