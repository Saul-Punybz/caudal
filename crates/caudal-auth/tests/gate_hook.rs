//! `GateHook` against a local mock service: what it sends, how it reads the
//! answer, its timeout and failure policy, and its cache.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use caudal_auth::{Action, GateHook, GateHookConfig, GateHookStats, OnError, ReplyMode};

const SECRET: &str = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";

#[derive(Clone)]
struct Mock {
    calls: Arc<AtomicUsize>,
    /// What the service does: status to answer, body, and a delay.
    status: StatusCode,
    body: &'static str,
    delay: Duration,
    last: Arc<parking_lot::Mutex<Option<serde_json::Value>>>,
}

async fn handler(State(m): State<Mock>, headers: HeaderMap, body: Bytes) -> (StatusCode, &'static str) {
    m.calls.fetch_add(1, Ordering::SeqCst);
    // Every call is signed per Standard Webhooks with the configured secret.
    let wh = standardwebhooks::Webhook::new(SECRET).unwrap();
    let mut h = http::HeaderMap::new();
    for k in ["webhook-id", "webhook-timestamp", "webhook-signature"] {
        h.insert(k, headers.get(k).expect(k).to_str().unwrap().parse().unwrap());
    }
    wh.verify(&body, &h).expect("valid signature");
    *m.last.lock() = Some(serde_json::from_slice(&body).unwrap());
    tokio::time::sleep(m.delay).await;
    (m.status, m.body)
}

async fn serve(status: StatusCode, body: &'static str, delay: Duration) -> (String, Mock) {
    let m = Mock { calls: Arc::default(), status, body, delay, last: Arc::default() };
    let app = Router::new().route("/gate", post(handler)).with_state(m.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/gate"), m)
}

fn cfg(url: &str) -> GateHookConfig {
    GateHookConfig {
        url: url.into(),
        secret: SECRET.into(),
        timeout: Duration::from_millis(300),
        on_error: OnError::Deny,
        reply: ReplyMode::Status,
        publish: true,
        play: true,
        allow_cache: Duration::from_secs(30),
        deny_cache: Duration::from_millis(200),
    }
}

fn hook(c: GateHookConfig) -> (GateHook, Arc<GateHookStats>) {
    let stats = Arc::new(GateHookStats::default());
    (GateHook::new(c, stats.clone()).unwrap(), stats)
}

fn count(stats: &GateHookStats, action: &str, outcome: &str) -> u64 {
    stats.calls().into_iter().find(|m| m.action == action && m.outcome == outcome).unwrap().count
}

#[tokio::test]
async fn a_2xx_allows_and_the_request_carries_stream_ip_and_token() {
    let (url, m) = serve(StatusCode::NO_CONTENT, "", Duration::ZERO).await;
    let (h, stats) = hook(cfg(&url));
    let ip = Some("203.0.113.9".parse().unwrap());
    assert_eq!(h.check(Action::Publish, "cam", Some("tok"), ip).await, Ok(()));
    let body = m.last.lock().clone().unwrap();
    assert_eq!(body["type"], "gate.publish");
    assert_eq!(body["data"], serde_json::json!({"stream": "cam", "ip": "203.0.113.9", "token": "tok"}));
    assert_eq!(count(&stats, "publish", "allow"), 1);
}

#[tokio::test]
async fn a_403_denies_with_the_service_reason() {
    let (url, _m) = serve(StatusCode::FORBIDDEN, r#"{"reason":"account suspended"}"#, Duration::ZERO).await;
    let (h, stats) = hook(cfg(&url));
    assert_eq!(h.check(Action::Play, "cam", None, None).await, Err("account suspended".into()));
    assert_eq!(count(&stats, "play", "deny"), 1);
}

#[tokio::test]
async fn a_slow_service_times_out_once_and_fails_closed() {
    let (url, m) = serve(StatusCode::OK, "", Duration::from_secs(3)).await;
    let (h, stats) = hook(cfg(&url));
    let t = Instant::now();
    let r = h.check(Action::Publish, "cam", None, None).await;
    let took = t.elapsed();
    assert_eq!(r, Err("authorization service unavailable".into()));
    assert!(took < Duration::from_millis(1500), "one 300 ms attempt, no retries: took {took:?}");
    assert_eq!(m.calls.load(Ordering::SeqCst), 1, "no retries");
    assert_eq!(count(&stats, "publish", "error_denied"), 1);
}

#[tokio::test]
async fn on_error_allow_lets_traffic_through_when_the_service_is_down() {
    let (h, stats) = hook(GateHookConfig { on_error: OnError::Allow, ..cfg("http://127.0.0.1:1/gate") });
    assert_eq!(h.check(Action::Play, "cam", None, None).await, Ok(()));
    assert_eq!(count(&stats, "play", "error_allowed"), 1);
}

#[tokio::test]
async fn a_5xx_is_a_failure_not_a_denial() {
    let (url, _m) = serve(StatusCode::INTERNAL_SERVER_ERROR, "", Duration::ZERO).await;
    let (h, stats) = hook(cfg(&url));
    assert!(h.check(Action::Play, "cam", None, None).await.is_err());
    assert_eq!(count(&stats, "play", "error_denied"), 1);
    assert_eq!(count(&stats, "play", "deny"), 0);
}

#[tokio::test]
async fn allows_are_cached_per_stream_ip_and_token() {
    let (url, m) = serve(StatusCode::OK, "", Duration::ZERO).await;
    let (h, stats) = hook(cfg(&url));
    for _ in 0..20 {
        h.check(Action::Play, "cam", Some("a"), None).await.unwrap();
    }
    assert_eq!(m.calls.load(Ordering::SeqCst), 1, "LL-HLS part requests must not each call out");
    assert_eq!(stats.cached()[1], ("play", 19));
    h.check(Action::Play, "cam", Some("b"), None).await.unwrap();
    h.check(Action::Play, "other", Some("a"), None).await.unwrap();
    h.check(Action::Publish, "cam", Some("a"), None).await.unwrap();
    assert_eq!(m.calls.load(Ordering::SeqCst), 4, "another token, stream or action is asked separately");
}

#[tokio::test]
async fn denials_expire_sooner_than_allows() {
    let (url, m) = serve(StatusCode::FORBIDDEN, "", Duration::ZERO).await;
    let (h, _) = hook(cfg(&url));
    assert!(h.check(Action::Play, "cam", None, None).await.is_err());
    assert!(h.check(Action::Play, "cam", None, None).await.is_err());
    assert_eq!(m.calls.load(Ordering::SeqCst), 1);
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(h.check(Action::Play, "cam", None, None).await.is_err());
    assert_eq!(m.calls.load(Ordering::SeqCst), 2, "the 200 ms deny cache expired");
}

#[tokio::test]
async fn concurrent_identical_requests_share_one_call() {
    let (url, m) = serve(StatusCode::OK, "", Duration::from_millis(150)).await;
    let (h, _) = hook(cfg(&url));
    let h = Arc::new(h);
    let tasks: Vec<_> = (0..50)
        .map(|_| {
            let h = h.clone();
            tokio::spawn(async move { h.check(Action::Play, "cam", Some("t"), None).await })
        })
        .collect();
    for t in tasks {
        assert_eq!(t.await.unwrap(), Ok(()));
    }
    assert_eq!(m.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn only_the_configured_actions_are_asked() {
    let (url, m) = serve(StatusCode::FORBIDDEN, "", Duration::ZERO).await;
    let (h, _) = hook(GateHookConfig { play: false, ..cfg(&url) });
    assert_eq!(h.check(Action::Play, "cam", None, None).await, Ok(()), "play not guarded");
    assert!(h.check(Action::Publish, "cam", None, None).await.is_err());
    assert_eq!(m.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn redirects_are_not_followed() {
    // A 307 would carry the token to wherever the service points.
    let (url, m) = serve(StatusCode::TEMPORARY_REDIRECT, "", Duration::ZERO).await;
    let (h, stats) = hook(cfg(&url));
    assert!(h.check(Action::Publish, "cam", Some("tok"), None).await.is_err());
    assert_eq!(m.calls.load(Ordering::SeqCst), 1);
    assert_eq!(count(&stats, "publish", "error_denied"), 1);
}

#[tokio::test]
async fn a_refusing_hook_refuses_everything() {
    let h = GateHook::refusing(Arc::default());
    assert!(h.check(Action::Publish, "cam", None, None).await.is_err());
    assert!(h.check(Action::Play, "cam", None, None).await.is_err());
}

/// How each reply mode reads a 2xx: `status` allows any of them,
/// `mistserver` only a body MistServer's `stringToBool` reads as true.
#[tokio::test]
async fn reply_modes_read_a_2xx_body_differently() {
    // (body, allowed with reply = "status", allowed with reply = "mistserver")
    let cases = [("true", true, true), ("false", true, false), ("", true, false), ("TRUE\n", true, true)];
    for (body, by_status, by_mist) in cases {
        let (url, m) = serve(StatusCode::OK, body, Duration::ZERO).await;
        for (reply, want) in [(ReplyMode::Status, by_status), (ReplyMode::MistServer, by_mist)] {
            for (action, name) in [(Action::Publish, "publish"), (Action::Play, "play")] {
                let (h, stats) = hook(GateHookConfig { reply, ..cfg(&url) });
                let r = h.check(action, "cam", Some("tok"), None).await;
                let what = format!("{reply:?} {action:?} 200 {body:?}");
                if want {
                    assert_eq!(r, Ok(()), "{what}");
                    assert_eq!(count(&stats, name, "allow"), 1, "{what}");
                } else {
                    assert_eq!(r, Err("refused by the authorization service".into()), "{what}");
                    assert_eq!(count(&stats, name, "deny"), 1, "{what}");
                }
                // Signed request, same body in both modes.
                assert_eq!(m.last.lock().clone().unwrap()["type"], format!("gate.{name}"));
            }
        }
    }
}

/// A non-2xx is a failure in both modes, and `on_error` decides it.
#[tokio::test]
async fn a_500_follows_on_error_in_both_reply_modes() {
    let (url, _m) = serve(StatusCode::INTERNAL_SERVER_ERROR, "true", Duration::ZERO).await;
    for reply in [ReplyMode::Status, ReplyMode::MistServer] {
        let (h, stats) = hook(GateHookConfig { reply, ..cfg(&url) });
        assert_eq!(h.check(Action::Publish, "cam", None, None).await, Err("authorization service unavailable".into()));
        assert_eq!(count(&stats, "publish", "error_denied"), 1, "{reply:?}");

        let (h, stats) = hook(GateHookConfig { reply, on_error: OnError::Allow, ..cfg(&url) });
        assert_eq!(h.check(Action::Play, "cam", None, None).await, Ok(()), "{reply:?}");
        assert_eq!(count(&stats, "play", "error_allowed"), 1, "{reply:?}");
    }
}

/// In `mistserver` mode a 403 still denies, and a `false` is cached like
/// any other denial: asked again only after `deny_cache`.
#[tokio::test]
async fn mistserver_denials_are_cached_and_403_still_denies() {
    let (url, m) = serve(StatusCode::OK, "false", Duration::ZERO).await;
    let (h, stats) = hook(GateHookConfig { reply: ReplyMode::MistServer, ..cfg(&url) });
    for _ in 0..3 {
        assert!(h.check(Action::Play, "cam", None, None).await.is_err());
    }
    assert_eq!(m.calls.load(Ordering::SeqCst), 1, "denial cached");
    assert_eq!(stats.cached()[1], ("play", 2));
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(h.check(Action::Play, "cam", None, None).await.is_err());
    assert_eq!(m.calls.load(Ordering::SeqCst), 2, "asked again after deny_cache");

    let (url, _m) = serve(StatusCode::FORBIDDEN, r#"{"reason":"banned"}"#, Duration::ZERO).await;
    let (h, _) = hook(GateHookConfig { reply: ReplyMode::MistServer, on_error: OnError::Allow, ..cfg(&url) });
    assert_eq!(h.check(Action::Publish, "cam", None, None).await, Err("banned".into()));
}
