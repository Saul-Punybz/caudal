//! `[auth.rewrite]` end to end, against the real binary: a routing service
//! running inside the test renames publishes and plays, and real clients
//! (ffmpeg over RTMP, srt-live-transmit, LL-HLS over HTTP) end up on the
//! stream it chose.
//!
//! Runs only with `CAUDAL_E2E=1` and ffmpeg on PATH, like `e2e.rs`.

// Drives real processes through Unix process groups, like e2e.rs.
#![cfg(unix)]

mod support;

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use parking_lot::Mutex;
use support::{Publisher, Server, have};

const SECRET: &str = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";

fn enabled() -> bool {
    if std::env::var("CAUDAL_E2E").is_err() {
        eprintln!("SKIP: set CAUDAL_E2E=1 to run end-to-end tests");
        return false;
    }
    if !have("ffmpeg") {
        eprintln!("SKIP: ffmpeg not on PATH");
        return false;
    }
    true
}

#[derive(Clone, Default)]
struct Routing {
    /// Calls by `"{type} {protocol} {stream}"`.
    calls: Arc<Mutex<HashMap<String, usize>>>,
    bad_signatures: Arc<AtomicUsize>,
}

impl Routing {
    fn calls(&self, key: &str) -> usize {
        self.calls.lock().get(key).copied().unwrap_or(0)
    }
}

/// Publishes of `alias` go to `real`; plays of `watch` come from `real`;
/// `nothing` gets an empty answer (refused); anything else is kept. `/slow`
/// never answers in time.
async fn route(State(r): State<Routing>, headers: HeaderMap, body: Bytes) -> (StatusCode, String) {
    let wh = standardwebhooks::Webhook::new(SECRET).unwrap();
    if wh.verify(&body, &headers).is_err() {
        r.bad_signatures.fetch_add(1, Ordering::SeqCst);
        return (StatusCode::UNAUTHORIZED, String::new());
    }
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let stream = v["data"]["stream"].as_str().unwrap_or_default().to_owned();
    let key = format!("{} {} {stream}", v["type"].as_str().unwrap(), v["data"]["protocol"].as_str().unwrap());
    *r.calls.lock().entry(key).or_default() += 1;
    match (v["type"].as_str().unwrap(), stream.as_str()) {
        ("rewrite.publish", "alias") => (StatusCode::OK, r#"{"stream":"real"}"#.into()),
        // MistServer-style plain-text answer.
        ("rewrite.play", "watch") => (StatusCode::OK, "real\n".into()),
        (_, "nothing") => (StatusCode::OK, String::new()),
        (_, other) => (StatusCode::OK, format!(r#"{{"stream":"{other}"}}"#)),
    }
}

async fn slow() -> &'static str {
    tokio::time::sleep(Duration::from_secs(5)).await;
    "never"
}

/// Starts the routing service on its own runtime thread; returns its port.
fn start_routing() -> (u16, Routing) {
    let r = Routing::default();
    let app = Router::new().route("/route", post(route)).route("/slow", post(slow)).with_state(r.clone());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            axum::serve(listener, app).await.unwrap();
        });
    });
    (port, r)
}

fn config(port: u16, path: &str, on_error: &str) -> String {
    format!(
        "\n[auth.rewrite]\nurl = \"http://127.0.0.1:{port}{path}\"\nsecret = \"{SECRET}\"\ntimeout_ms = 300\non_error = \"{on_error}\"\n"
    )
}

#[test]
fn publish_and_play_under_other_names() {
    if !enabled() {
        return;
    }
    let (port, routing) = start_routing();
    let s = Server::start_with(&config(port, "/route", "deny"));

    // RTMP publish to `alias` lands on `real`.
    let _publ = Publisher::rtmp(&s.rtmp_url("alias"), 40);
    s.wait_until("/api/v1/streams/real", Duration::from_secs(20), |b| b.contains("\"h264\""));
    assert_eq!(s.get("/api/v1/streams/alias").unwrap().0, 404, "nothing is published under the alias");
    assert_eq!(routing.calls("rewrite.publish rtmp alias"), 1);

    // LL-HLS play of `watch` serves `real`, and the master keeps the viewer
    // on its own name.
    let playlist = s.wait_until("/hls/watch/index.m3u8", Duration::from_secs(20), |b| b.contains("#EXT-X-PART"));
    assert!(playlist.contains("INDEPENDENT=YES"), "{playlist}");
    let init = s.get_bytes("/hls/watch/init.mp4");
    assert_eq!(&init[4..8], b"ftyp");
    let (code, master) = s.get("/hls/watch/master.m3u8").unwrap();
    assert_eq!(code, 200);
    assert!(master.contains("../watch/index.m3u8") && !master.contains("../real/"), "{master}");
    // Every one of those requests had the same protocol, name, IP and no
    // token: answered from the cache after the first call.
    assert_eq!(routing.calls("rewrite.play hls watch"), 1, "{:?}", routing.calls.lock());

    // SRT play of `watch` gets MPEG-TS from `real`.
    if have("srt-live-transmit") {
        let out = tempfile::NamedTempFile::new().unwrap();
        let mut viewer = Command::new("srt-live-transmit")
            .args(["-q", &format!("srt://127.0.0.1:{}?streamid=play/watch", s.srt), "file://con"])
            .stdout(Stdio::from(out.reopen().unwrap()))
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_secs(4));
        let _ = viewer.kill();
        let _ = viewer.wait();
        let ts = std::fs::read(out.path()).unwrap();
        assert!(ts.len() > 188 * 100 && ts[0] == 0x47 && ts[188] == 0x47, "{} bytes of TS", ts.len());
        assert_eq!(routing.calls("rewrite.play srt watch"), 1);
    } else {
        eprintln!("SKIP (SRT play part): srt-live-transmit not on PATH");
    }

    // An empty answer refuses: a publish to `nothing` never appears, a
    // play of it is a 404.
    let _denied = Publisher::rtmp(&s.rtmp_url("nothing"), 10);
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(s.get("/api/v1/streams/nothing").unwrap().0, 404);
    assert_eq!(routing.calls("rewrite.publish rtmp nothing"), 1);
    assert_eq!(s.get("/hls/nothing/index.m3u8").unwrap().0, 404);
    assert_eq!(routing.calls("rewrite.play hls nothing"), 1);

    let (_, metrics) = s.get("/metrics").unwrap();
    assert!(metrics.contains("caudal_rewrite_hook_calls_total{action=\"publish\",outcome=\"rewrite\"} 1"), "{metrics}");
    assert!(metrics.contains("caudal_rewrite_hook_calls_total{action=\"publish\",outcome=\"deny\"} 1"));
    assert_eq!(routing.bad_signatures.load(Ordering::SeqCst), 0, "every call was signed with the secret");
}

#[test]
fn a_routing_service_that_times_out_follows_on_error() {
    if !enabled() {
        return;
    }
    let (port, _routing) = start_routing();

    // on_error = "allow": the publish goes on under the name it asked for.
    let s = Server::start_with(&config(port, "/slow", "allow"));
    let _publ = Publisher::rtmp(&s.rtmp_url("kept"), 20);
    s.wait_until("/api/v1/streams/kept", Duration::from_secs(20), |b| b.contains("\"h264\""));
    let (_, metrics) = s.get("/metrics").unwrap();
    assert!(metrics.contains("caudal_rewrite_hook_calls_total{action=\"publish\",outcome=\"error_allowed\"} 1"));
    drop(s);

    // on_error = "deny" (the default): refused.
    let s = Server::start_with(&config(port, "/slow", "deny"));
    let _publ = Publisher::rtmp(&s.rtmp_url("kept"), 10);
    std::thread::sleep(Duration::from_secs(4));
    assert_eq!(s.get("/api/v1/streams/kept").unwrap().0, 404);
    let (_, metrics) = s.get("/metrics").unwrap();
    assert!(metrics.contains("caudal_rewrite_hook_calls_total{action=\"publish\",outcome=\"error_denied\"} 1"));
}
