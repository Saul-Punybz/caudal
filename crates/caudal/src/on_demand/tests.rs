//! On-demand pulls against a real source: ffmpeg serving MPEG-TS over HTTP
//! (`-listen 1`, one client per process, so a second pull of the same
//! source would fail), behind an on-demand `[[http.pull]]` or a URL from a
//! test `[auth.source]` service. Viewers are subscribers on the registry,
//! asking through `Registry::get_or_demand` as every output does.

use std::process::{Child, Command, Stdio};
use std::sync::atomic::AtomicUsize;

use caudal_core::StartAt;

use super::*;

const SECRET: &str = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";

fn have_ffmpeg() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success())
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// ffmpeg serving one client at `url`; killed on drop.
struct Ffmpeg(Child);

impl Ffmpeg {
    fn serve(url: &str) -> Self {
        let child = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-re", "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=25"])
            .args(["-c:v", "libx264", "-preset", "veryfast", "-g", "25", "-t", "120", "-f", "mpegts", "-listen", "1"])
            .arg(url)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ffmpeg");
        Self(child)
    }
}

impl Drop for Ffmpeg {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn wait_for(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    f()
}

fn http_entry(stream: &str, url: &str, idle: Duration) -> HashMap<String, Entry> {
    let source = Source::Http(caudal_http_pull::HttpPull { stream: stream.into(), url: url.into() });
    HashMap::from([(stream.to_owned(), Entry { source, idle })])
}

fn row(stream: &str, url: &str) -> crate::config_edit::EntryJson {
    crate::config_edit::EntryJson { stream: stream.into(), url: url.into(), section: "[[http.pull]]" }
}

/// The rows `GET /api/v1/pulls` shows, with `web` configured.
fn rows(od: &OnDemand) -> Vec<PullRow> {
    od.rows(vec![row("web", "http://x/stream.ts")])
}

fn state_of(od: &OnDemand, stream: &str) -> Option<&'static str> {
    rows(od).into_iter().find(|p| p.entry.stream == stream).map(|p| p.state)
}

#[test]
fn source_urls() {
    let kind = |u: &str| Source::from_url("s", u).map(|s| s.kind());
    assert_eq!(kind("srt://10.0.0.5:9000"), Ok("srt"));
    assert_eq!(kind("udp://239.1.1.1:5000"), Ok("udp"));
    assert_eq!(kind("rtp://:5004"), Ok("udp"));
    assert_eq!(kind("http://10.0.0.5:8080/stream.ts"), Ok("http"));
    assert_eq!(kind("https://cdn.example/live/index.m3u8?t=1"), Ok("hls"));
    assert_eq!(kind("rtmp://h/live/key"), Ok("rtmp"));
    assert_eq!(kind("rtsps://cam.example/stream1"), Ok("rtsp"));
    assert!(kind("omt://h:1").unwrap_err().contains("not a kind of source"));
    assert!(kind("rtsp://").is_err());
    assert!(kind("nonsense").is_err());
}

#[test]
fn urls_are_redacted() {
    assert_eq!(redact("rtsp://admin:hunter2@10.0.0.5/live"), "rtsp://10.0.0.5/live");
    assert_eq!(redact("srt://h:9000?passphrase=secretsecret"), "srt://h:9000");
    assert_eq!(redact("not a url"), "<not a url>");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_stream_starts_nothing() {
    let registry = Registry::new();
    let od = OnDemand::install(&registry, BufferConfig::default());
    assert!(registry.get_or_demand("nope").await.is_none());
    assert_eq!(od.counters.starts.load(Ordering::Relaxed), 0);
    assert!(od.rows(Vec::new()).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn first_viewers_start_one_pull_and_idle_stops_it() {
    if !have_ffmpeg() {
        eprintln!("SKIP: ffmpeg not installed");
        return;
    }
    let url = format!("http://127.0.0.1:{}/stream.ts", free_port());
    let registry = Registry::new();
    let od = OnDemand::install(&registry, BufferConfig::default());
    od.configure(http_entry("web", &url, Duration::from_secs(2)));
    let _source = Ffmpeg::serve(&url);
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(registry.get("web").is_none(), "nothing runs before a viewer asks");
    assert_eq!(state_of(&od, "web"), Some("idle"));
    // An always-on row: no stream, so connecting.
    let other = od.rows(vec![row("tv", "udp://:5000")]);
    assert_eq!((other[0].on_demand, other[0].state), (false, "connecting"));

    // Eight viewers at once: one pull (ffmpeg would refuse a second client).
    let asks: Vec<_> = (0..8)
        .map(|_| {
            let r = registry.clone();
            tokio::spawn(async move { r.get_or_demand("web").await })
        })
        .collect();
    let mut streams = Vec::new();
    for a in asks {
        streams.push(a.await.unwrap().expect("every viewer gets the stream"));
    }
    assert!(streams.iter().all(|s| Arc::ptr_eq(s, &streams[0])));
    assert_eq!(od.counters.starts.load(Ordering::Relaxed), 1);
    assert!(streams[0].stats().frames_in > 0, "handed out with media");
    drop(streams);
    assert_eq!(state_of(&od, "web"), Some("live"));

    // A viewer keeps it up past idle_secs.
    let viewer = registry.subscribe("web", StartAt::LiveEdge).unwrap();
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(registry.get("web").is_some(), "watched: still pulling");
    let json = rows(&od).into_iter().find(|p| p.entry.stream == "web").unwrap();
    assert_eq!((json.on_demand, json.idle_secs, json.viewers), (true, Some(2), 1));
    let v = serde_json::to_value(&json).unwrap();
    assert_eq!(v["section"], "[[http.pull]]");
    assert_eq!(v["state"], "live");

    // The last viewer leaves: stopped after idle_secs.
    drop(viewer);
    let t0 = Instant::now();
    assert!(wait_for(Duration::from_secs(8), || registry.get("web").is_none()).await, "stopped when idle");
    assert!(t0.elapsed() >= Duration::from_millis(1500), "not before idle_secs ({:?})", t0.elapsed());
    assert!(wait_for(Duration::from_secs(3), || state_of(&od, "web") == Some("idle")).await);
    assert_eq!(od.counters.idle_stops.load(Ordering::Relaxed), 1);
    let mut metrics = String::new();
    od.render_metrics(&mut metrics);
    assert!(metrics.contains("caudal_on_demand_starts_total 1"), "{metrics}");
    assert!(metrics.contains("caudal_on_demand_idle_stops_total 1"), "{metrics}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_source_without_media_is_not_available_and_backs_off() {
    // Nothing listens there.
    let url = format!("http://127.0.0.1:{}/stream.ts", free_port());
    let registry = Registry::new();
    let od = OnDemand::install(&registry, BufferConfig::default());
    od.configure(http_entry("dead", &url, Duration::from_secs(2)));
    let t0 = Instant::now();
    assert!(registry.get_or_demand("dead").await.is_none());
    assert!(t0.elapsed() >= FIRST_MEDIA, "waited for the first media");
    assert_eq!(od.counters.failures.load(Ordering::Relaxed), 1);
    // Backing off: answered at once, nothing started.
    assert!(wait_for(Duration::from_secs(3), || od.pulls.lock().is_empty()).await);
    let t1 = Instant::now();
    assert!(registry.get_or_demand("dead").await.is_none());
    assert!(t1.elapsed() < Duration::from_secs(1));
    assert_eq!(od.counters.starts.load(Ordering::Relaxed), 1);
}

/// `[auth.source]` service: `cam` comes from `url`, everything else has no
/// source. Counts calls; rejects bad signatures.
async fn start_hook(url: String) -> (u16, Arc<AtomicUsize>) {
    use axum::body::Bytes;
    use axum::http::{HeaderMap, StatusCode};
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let app = axum::Router::new().route(
        "/source",
        axum::routing::post(move |headers: HeaderMap, body: Bytes| {
            let (c, url) = (c.clone(), url.clone());
            async move {
                if standardwebhooks::Webhook::new(SECRET).unwrap().verify(&body, &headers).is_err() {
                    return (StatusCode::UNAUTHORIZED, String::new());
                }
                let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(v["type"], "stream.source");
                c.fetch_add(1, Ordering::SeqCst);
                match v["data"]["stream"].as_str() {
                    Some("cam") => (StatusCode::OK, serde_json::json!({ "source": url }).to_string()),
                    _ => (StatusCode::OK, String::new()),
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (port, calls)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_source_hook_names_the_source() {
    if !have_ffmpeg() {
        eprintln!("SKIP: ffmpeg not installed");
        return;
    }
    let url = format!("http://127.0.0.1:{}/cam.ts", free_port());
    let (port, calls) = start_hook(url.clone()).await;
    let registry = Registry::new();
    let od = OnDemand::install(&registry, BufferConfig::default());
    let hook = caudal_auth::SourceHook::new(
        caudal_auth::SourceHookConfig {
            url: format!("http://127.0.0.1:{port}/source"),
            secret: SECRET.into(),
            timeout: Duration::from_secs(2),
            cache: Duration::from_secs(10),
            deny_cache: Duration::from_secs(5),
        },
        od.hook_stats.clone(),
    )
    .unwrap();
    od.set_hook(Some(SourceHook { hook: Arc::new(hook), idle: Duration::from_secs(1) }));

    // No such stream: one call, nothing started; asked again, cached.
    assert!(registry.get_or_demand("nope").await.is_none());
    assert!(registry.get_or_demand("nope").await.is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(od.counters.starts.load(Ordering::Relaxed), 0);
    assert_eq!(od.hook_stats.cached(), 1);

    let _source = Ffmpeg::serve(&url);
    tokio::time::sleep(Duration::from_millis(700)).await;
    let stream = registry.get_or_demand("cam").await.expect("pulled from the hook's source");
    assert!(stream.stats().frames_in > 0);
    let json = od.rows(Vec::new()).into_iter().find(|p| p.entry.stream == "cam").unwrap();
    assert_eq!((json.entry.section, json.state), ("[auth.source]", "live"));
    assert_eq!(json.entry.url, url);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    drop(stream);
    assert!(wait_for(Duration::from_secs(6), || registry.get("cam").is_none()).await, "stopped when idle");
    assert!(
        wait_for(Duration::from_secs(3), || od.rows(Vec::new()).is_empty()).await,
        "a hook pull is listed only while it runs"
    );
    let mut metrics = String::new();
    od.render_metrics(&mut metrics);
    assert!(metrics.contains("caudal_source_hook_calls_total{outcome=\"source\"} 1"), "{metrics}");
    assert!(metrics.contains("caudal_source_hook_calls_total{outcome=\"none\"} 1"), "{metrics}");
}
