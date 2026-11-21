//! `[auth.hook]` with `reply = "mistserver"` end to end, against the real
//! binary: an authorization service running inside the test answers the
//! way a MistServer blocking trigger handler does (`200` with `true` or
//! `false`), and a real RTMP publish (ffmpeg) and LL-HLS plays are let in
//! or refused by the body alone.
//!
//! Runs only with `CAUDAL_E2E=1` and ffmpeg on PATH, like `e2e.rs`.

// Drives real processes through Unix process groups, like e2e.rs.
#![cfg(unix)]

mod support;

use std::collections::HashMap;
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
struct Service {
    /// Calls by `"{type} {stream} {token}"`.
    calls: Arc<Mutex<HashMap<String, usize>>>,
    bad_signatures: Arc<AtomicUsize>,
}

/// Always `200`: publishes of `live` get `true\n`, every other publish
/// `false`; plays with token `good` get `1`, with `bad` get `0`, without
/// a token an empty body.
async fn gate(State(s): State<Service>, headers: HeaderMap, body: Bytes) -> (StatusCode, &'static str) {
    let wh = standardwebhooks::Webhook::new(SECRET).unwrap();
    if wh.verify(&body, &headers).is_err() {
        s.bad_signatures.fetch_add(1, Ordering::SeqCst);
        return (StatusCode::UNAUTHORIZED, "");
    }
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let (kind, stream) = (v["type"].as_str().unwrap(), v["data"]["stream"].as_str().unwrap());
    let token = v["data"]["token"].as_str().unwrap_or("-");
    *s.calls.lock().entry(format!("{kind} {stream} {token}")).or_default() += 1;
    let answer = match (kind, stream, token) {
        ("gate.publish", "live", _) => "true\n",
        ("gate.publish", _, _) => "false",
        ("gate.play", _, "good") => "1",
        ("gate.play", _, "bad") => "0",
        _ => "",
    };
    (StatusCode::OK, answer)
}

fn start_service() -> (u16, Service) {
    let s = Service::default();
    let app = Router::new().route("/gate", post(gate)).with_state(s.clone());
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
    (port, s)
}

#[test]
fn mistserver_style_answers_allow_and_deny_publish_and_play() {
    if !enabled() {
        return;
    }
    let (port, service) = start_service();
    // on_error = "allow" so that a refusal below can only come from the body.
    let s = Server::start_with(&format!(
        "\n[auth.hook]\nurl = \"http://127.0.0.1:{port}/gate\"\nsecret = \"{SECRET}\"\ntimeout_ms = 500\non_error = \"allow\"\nreply = \"mistserver\"\n"
    ));

    // `200 true\n` lets the publish in.
    let _live = Publisher::rtmp(&s.rtmp_url("live"), 40);
    s.wait_until("/api/v1/streams/live", Duration::from_secs(20), |b| b.contains("\"h264\""));

    // `200 false` refuses it.
    let _refused = Publisher::rtmp(&s.rtmp_url("refused"), 10);
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(s.get("/api/v1/streams/refused").unwrap().0, 404, "a 200 false publish never appears");
    assert_eq!(service.calls.lock().get("gate.publish refused -").copied(), Some(1));

    // `200 1` lets a viewer play; `200 0` and an empty `200` do not.
    let playlist =
        s.wait_until("/hls/live/index.m3u8?token=good", Duration::from_secs(20), |b| b.contains("#EXT-X-PART"));
    assert!(playlist.contains("#EXTM3U"), "{playlist}");
    assert_eq!(s.get("/hls/live/index.m3u8?token=bad").unwrap().0, 403);
    assert_eq!(s.get("/hls/live/index.m3u8").unwrap().0, 403);

    let (_, metrics) = s.get("/metrics").unwrap();
    for line in [
        "caudal_auth_hook_calls_total{action=\"publish\",outcome=\"allow\"} 1",
        "caudal_auth_hook_calls_total{action=\"publish\",outcome=\"deny\"} 1",
        "caudal_auth_hook_calls_total{action=\"play\",outcome=\"deny\"} 2",
        "caudal_auth_hook_calls_total{action=\"publish\",outcome=\"error_allowed\"} 0",
        "caudal_auth_hook_calls_total{action=\"play\",outcome=\"error_allowed\"} 0",
    ] {
        assert!(metrics.contains(line), "missing {line}\n{metrics}");
    }
    assert_eq!(service.bad_signatures.load(Ordering::SeqCst), 0, "every call was signed with the secret");
}
