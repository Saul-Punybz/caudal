//! `Hooks::emit` delivery against a local axum receiver: signature is
//! verifiable with `standardwebhooks::Webhook::verify`, 5xx retries with
//! backoff, 4xx does not retry. No network beyond 127.0.0.1.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use caudal_auth::{HookEvent, Hooks, HooksConfig};
use tokio::sync::Notify;

fn make_secret() -> String {
    use base64::Engine;
    let raw = [7u8; 24];
    format!("whsec_{}", base64::engine::general_purpose::STANDARD.encode(raw))
}

type Captured = Option<(HeaderMap, Vec<u8>)>;

#[derive(Clone)]
struct CaptureState {
    captured: Arc<std::sync::Mutex<Captured>>,
    notify: Arc<Notify>,
}

async fn capture_handler(State(state): State<CaptureState>, headers: HeaderMap, body: axum::body::Bytes) -> StatusCode {
    *state.captured.lock().unwrap() = Some((headers, body.to_vec()));
    state.notify.notify_one();
    StatusCode::OK
}

async fn spawn_capture_server() -> (String, CaptureState) {
    let state = CaptureState { captured: Arc::new(std::sync::Mutex::new(None)), notify: Arc::new(Notify::new()) };
    let app = axum::Router::new().route("/hook", post(capture_handler)).with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/hook"), state)
}

#[tokio::test]
async fn webhook_is_signed_and_shaped_per_spec() {
    let secret = make_secret();
    let (url, state) = spawn_capture_server().await;
    let hooks = Hooks::new(Some(HooksConfig { urls: vec![url], secret: secret.clone() }));

    hooks.emit(HookEvent::StreamStarted { stream: "live-main".to_string() });

    tokio::time::timeout(Duration::from_secs(5), state.notify.notified()).await.expect("webhook not delivered");

    let (headers, body) = state.captured.lock().unwrap().take().expect("no request captured");
    let webhook = standardwebhooks::Webhook::new(&secret).unwrap();
    webhook.verify(&body, &headers).expect("signature must verify");

    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["type"], "stream.started");
    assert_eq!(value["data"]["stream"], "live-main");
    assert!(value["timestamp"].as_str().unwrap().contains('T'));
}

#[derive(Clone)]
struct FlakyState {
    attempts: Arc<AtomicUsize>,
    fail_first: usize,
    notify: Arc<Notify>,
}

async fn flaky_handler(State(state): State<FlakyState>) -> StatusCode {
    let n = state.attempts.fetch_add(1, Ordering::SeqCst) + 1;
    if n <= state.fail_first {
        StatusCode::INTERNAL_SERVER_ERROR
    } else {
        state.notify.notify_one();
        StatusCode::OK
    }
}

async fn spawn_flaky_server(fail_first: usize) -> (String, FlakyState) {
    let state = FlakyState { attempts: Arc::new(AtomicUsize::new(0)), fail_first, notify: Arc::new(Notify::new()) };
    let app = axum::Router::new().route("/hook", post(flaky_handler)).with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/hook"), state)
}

// Real time, not paused: reqwest's own request timeout and hyper's
// connection-pool timers race badly against a manually-paused clock on a
// single-threaded runtime that also hosts the loopback server, so this
// exercises the real 1s/5s backoff (~6s wall clock) instead.
#[tokio::test]
async fn retries_on_5xx_until_success() {
    let secret = make_secret();
    // Fails twice (500, 500) then succeeds on the 3rd attempt.
    let (url, state) = spawn_flaky_server(2).await;
    let hooks = Hooks::new(Some(HooksConfig { urls: vec![url], secret }));

    hooks.emit(HookEvent::StreamEnded { stream: "live-main".to_string() });

    tokio::time::timeout(Duration::from_secs(30), state.notify.notified()).await.expect("delivery never succeeded");

    assert_eq!(state.attempts.load(Ordering::SeqCst), 3);
}

#[derive(Clone)]
struct AlwaysStatus {
    attempts: Arc<AtomicUsize>,
    status: StatusCode,
}

async fn always_status_handler(State(state): State<AlwaysStatus>) -> StatusCode {
    state.attempts.fetch_add(1, Ordering::SeqCst);
    state.status
}

async fn spawn_status_server(status: StatusCode) -> (String, AlwaysStatus) {
    let state = AlwaysStatus { attempts: Arc::new(AtomicUsize::new(0)), status };
    let app = axum::Router::new().route("/hook", post(always_status_handler)).with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/hook"), state)
}

#[tokio::test]
async fn client_error_is_not_retried() {
    let secret = make_secret();
    let (url, state) = spawn_status_server(StatusCode::BAD_REQUEST).await;
    let hooks = Hooks::new(Some(HooksConfig { urls: vec![url], secret }));

    hooks.emit(HookEvent::StreamStarted { stream: "live-main".to_string() });

    // The 4xx path returns immediately (no backoff sleep at all), so a
    // short real wait is enough to observe the single attempt and confirm
    // nothing further is scheduled.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(state.attempts.load(Ordering::SeqCst), 1);

    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(state.attempts.load(Ordering::SeqCst), 1, "4xx must not be retried");
}

#[tokio::test]
async fn emit_without_hooks_config_is_a_noop() {
    let hooks = Hooks::new(None);
    // Must not panic, must not spawn anything observable.
    hooks.emit(HookEvent::StreamStarted { stream: "live-main".to_string() });
    tokio::time::sleep(Duration::from_millis(50)).await;
}
