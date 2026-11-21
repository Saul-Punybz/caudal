//! `RewriteHook` against a local routing service: what it sends, how it
//! reads the answer (JSON and MistServer-style plain text), denial,
//! timeout and `on_error`, and its cache.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use caudal_auth::{Action, OnError, RewriteHook, RewriteHookConfig, RewriteHookStats};

const SECRET: &str = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";

#[derive(Clone)]
struct Mock {
    calls: Arc<AtomicUsize>,
    delay: Duration,
    last: Arc<parking_lot::Mutex<Option<serde_json::Value>>>,
}

/// A routing service: `alias` -> `real` (JSON), `plain` -> `real` (plain
/// text, MistServer style), `short` -> `real` remembered 0 s, `vip` -> a
/// per-viewer answer, `nothing` -> empty (deny), `banned` -> 403, `broken`
/// -> 500, anything else kept.
async fn handler(State(m): State<Mock>, headers: HeaderMap, body: Bytes) -> (StatusCode, String) {
    m.calls.fetch_add(1, Ordering::SeqCst);
    let wh = standardwebhooks::Webhook::new(SECRET).unwrap();
    let mut h = http::HeaderMap::new();
    for k in ["webhook-id", "webhook-timestamp", "webhook-signature"] {
        h.insert(k, headers.get(k).expect(k).to_str().unwrap().parse().unwrap());
    }
    wh.verify(&body, &h).expect("valid signature");
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    *m.last.lock() = Some(v.clone());
    tokio::time::sleep(m.delay).await;
    let stream = v["data"]["stream"].as_str().unwrap().to_owned();
    match stream.as_str() {
        "alias" => (StatusCode::OK, r#"{"stream":"real"}"#.into()),
        "plain" => (StatusCode::OK, "real\n".into()),
        "short" => (StatusCode::OK, r#"{"stream":"real","cache_secs":0}"#.into()),
        "vip" => {
            let to = if v["data"]["ip"] == "203.0.113.1" { "vip-hd" } else { "vip-sd" };
            (StatusCode::OK, format!(r#"{{"stream":"{to}"}}"#))
        }
        "nothing" => (StatusCode::OK, String::new()),
        "banned" => (StatusCode::FORBIDDEN, r#"{"reason":"no route for you"}"#.into()),
        "broken" => (StatusCode::INTERNAL_SERVER_ERROR, String::new()),
        other => (StatusCode::OK, format!(r#"{{"stream":"{other}"}}"#)),
    }
}

async fn serve(delay: Duration) -> (String, Mock) {
    let m = Mock { calls: Arc::default(), delay, last: Arc::default() };
    let app = Router::new().route("/route", post(handler)).with_state(m.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/route"), m)
}

fn cfg(url: &str) -> RewriteHookConfig {
    RewriteHookConfig {
        url: url.into(),
        secret: SECRET.into(),
        timeout: Duration::from_millis(300),
        on_error: OnError::Deny,
        publish: true,
        play: true,
        cache: Duration::from_secs(30),
        deny_cache: Duration::from_millis(200),
    }
}

fn hook(c: RewriteHookConfig) -> (RewriteHook, Arc<RewriteHookStats>) {
    let stats = Arc::new(RewriteHookStats::default());
    (RewriteHook::new(c, stats.clone()).unwrap(), stats)
}

fn count(stats: &RewriteHookStats, action: &str, outcome: &str) -> u64 {
    stats.calls().into_iter().find(|m| m.action == action && m.outcome == outcome).unwrap().count
}

#[tokio::test]
async fn rewrites_and_the_request_carries_protocol_stream_ip_and_token() {
    let (url, m) = serve(Duration::ZERO).await;
    let (h, stats) = hook(cfg(&url));
    let ip = Some("203.0.113.9".parse().unwrap());
    assert_eq!(h.rewrite(Action::Publish, "rtmp", "alias", Some("tok"), ip).await, Ok("real".into()));
    let body = m.last.lock().clone().unwrap();
    assert_eq!(body["type"], "rewrite.publish");
    assert_eq!(
        body["data"],
        serde_json::json!({"stream": "alias", "protocol": "rtmp", "ip": "203.0.113.9", "token": "tok"})
    );
    assert_eq!(h.rewrite(Action::Play, "hls", "plain", None, None).await, Ok("real".into()), "plain text");
    assert_eq!(h.rewrite(Action::Play, "hls", "cam", None, None).await, Ok("cam".into()));
    assert_eq!(count(&stats, "publish", "rewrite"), 1);
    assert_eq!(count(&stats, "play", "rewrite"), 1);
    assert_eq!(count(&stats, "play", "keep"), 1);
}

#[tokio::test]
async fn an_empty_answer_or_a_403_denies() {
    let (url, _m) = serve(Duration::ZERO).await;
    let (h, stats) = hook(cfg(&url));
    assert!(h.rewrite(Action::Publish, "srt", "nothing", None, None).await.is_err());
    assert_eq!(h.rewrite(Action::Play, "whep", "banned", None, None).await, Err("no route for you".into()));
    assert_eq!(count(&stats, "publish", "deny"), 1);
    assert_eq!(count(&stats, "play", "deny"), 1);
}

#[tokio::test]
async fn a_slow_service_times_out_once_and_on_error_decides() {
    let (url, m) = serve(Duration::from_secs(3)).await;
    let (h, stats) = hook(cfg(&url));
    let t = Instant::now();
    assert_eq!(
        h.rewrite(Action::Publish, "rtmp", "alias", None, None).await,
        Err("routing service unavailable".into())
    );
    assert!(t.elapsed() < Duration::from_millis(1500), "one 300 ms attempt: took {:?}", t.elapsed());
    assert_eq!(m.calls.load(Ordering::SeqCst), 1, "no retries");
    assert_eq!(count(&stats, "publish", "error_denied"), 1);

    let (h, stats) = hook(RewriteHookConfig { on_error: OnError::Allow, ..cfg(&url) });
    assert_eq!(h.rewrite(Action::Play, "hls", "alias", None, None).await, Ok("alias".into()), "kept as asked");
    assert_eq!(count(&stats, "play", "error_allowed"), 1);
    let (url, _m) = serve(Duration::ZERO).await;
    let (h, _) = hook(RewriteHookConfig { on_error: OnError::Allow, ..cfg(&url) });
    assert_eq!(h.rewrite(Action::Play, "hls", "broken", None, None).await, Ok("broken".into()), "a 500 too");
}

#[tokio::test]
async fn answers_are_cached_per_viewer_and_shared_in_flight() {
    let (url, m) = serve(Duration::from_millis(100)).await;
    let (h, stats) = hook(cfg(&url));
    let h = Arc::new(h);
    let a: Option<std::net::IpAddr> = Some("203.0.113.1".parse().unwrap());
    let b: Option<std::net::IpAddr> = Some("203.0.113.2".parse().unwrap());
    // Ten identical requests at once: one call.
    let mut tasks = Vec::new();
    for _ in 0..10 {
        let h = h.clone();
        tasks.push(tokio::spawn(async move { h.rewrite(Action::Play, "hls", "vip", None, a).await }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), Ok("vip-hd".into()));
    }
    assert_eq!(m.calls.load(Ordering::SeqCst), 1);
    // Cached: no second call.
    assert_eq!(h.rewrite(Action::Play, "hls", "vip", None, a).await, Ok("vip-hd".into()));
    assert_eq!(m.calls.load(Ordering::SeqCst), 1);
    assert!(stats.cached()[1].1 >= 1);
    // Another viewer is asked for separately and gets its own answer.
    assert_eq!(h.rewrite(Action::Play, "hls", "vip", None, b).await, Ok("vip-sd".into()));
    assert_eq!(m.calls.load(Ordering::SeqCst), 2);
    // So is another protocol.
    assert_eq!(h.rewrite(Action::Play, "srt", "vip", None, a).await, Ok("vip-hd".into()));
    assert_eq!(m.calls.load(Ordering::SeqCst), 3);
    // `cache_secs: 0` is never remembered.
    h.rewrite(Action::Play, "hls", "short", None, a).await.unwrap();
    h.rewrite(Action::Play, "hls", "short", None, a).await.unwrap();
    assert_eq!(m.calls.load(Ordering::SeqCst), 5);
}

#[tokio::test]
async fn only_the_configured_actions_are_asked() {
    let (url, m) = serve(Duration::ZERO).await;
    let (h, _) = hook(RewriteHookConfig { play: false, ..cfg(&url) });
    assert_eq!(h.rewrite(Action::Play, "hls", "alias", None, None).await, Ok("alias".into()));
    assert_eq!(m.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_hook_that_could_not_start_refuses() {
    let h = RewriteHook::refusing(true, true, Arc::default());
    assert!(h.rewrite(Action::Publish, "rtmp", "cam", None, None).await.is_err());
}
