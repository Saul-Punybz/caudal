//! End to end: a `Registry` publisher that stops sending keyframes trips
//! `no_keyframe`, delivered as one signed `alert` then one signed `resolved`
//! to a local HTTP receiver bound on port 0. Real wall-clock time (not
//! `tokio::time::pause`): the delivery worker's own timers would race a
//! manually-paused clock on the same single-threaded runtime, same reason
//! `caudal-auth/tests/hooks.rs` uses real time for its retry test.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use caudal_core::{BufferConfig, Codec, Frame, Registry, TrackId, TrackInfo, VideoParams};
use caudal_health::HealthConfig;
use tokio::sync::Notify;

fn make_secret() -> String {
    use base64::Engine;
    let raw = [9u8; 24];
    format!("whsec_{}", base64::engine::general_purpose::STANDARD.encode(raw))
}

type CapturedEvents = Vec<(HeaderMap, Vec<u8>)>;

#[derive(Clone, Default)]
struct Captured {
    events: Arc<std::sync::Mutex<CapturedEvents>>,
    notify: Arc<Notify>,
    count: Arc<AtomicUsize>,
}

async fn capture_handler(State(state): State<Captured>, headers: HeaderMap, body: axum::body::Bytes) -> StatusCode {
    state.events.lock().unwrap().push((headers, body.to_vec()));
    state.count.fetch_add(1, Ordering::SeqCst);
    state.notify.notify_waiters();
    StatusCode::OK
}

/// Binds on port 0 and reads back the address actually assigned, rather
/// than probing a free port and racing to rebind it.
async fn spawn_receiver() -> (String, Captured) {
    let state = Captured::default();
    let app = axum::Router::new().route("/hook", post(capture_handler)).with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/hook"), state)
}

// The rule thresholds below (1s) are what is under test; this timeout only
// guards against a delivery that never happens. It was 90 s, blamed on load;
// the real cause was a lost wake-up in this helper (fixed below).
async fn wait_for(state: &Captured, n: usize) {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            // Register before checking: `notify_waiters` only wakes waiters
            // that already exist, so checking first and then waiting lost
            // any delivery that landed in between (CI hung to the timeout).
            let notified = state.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if state.count.load(Ordering::SeqCst) >= n {
                return;
            }
            notified.await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        let got = state.count.load(Ordering::SeqCst);
        let events = state.events.lock().unwrap().clone();
        panic!("webhook(s) not delivered in time: wanted {n}, got {got}: {events:?}")
    });
}

fn video_track() -> TrackInfo {
    TrackInfo {
        id: TrackId(0),
        codec: Codec::H264,
        timescale: 90_000,
        init: Default::default(),
        lang: None,
        video: Some(VideoParams { width: 16, height: 16, fps: None }),
        audio: None,
    }
}

#[tokio::test]
async fn no_keyframe_alert_then_resolved_with_valid_signatures() {
    let secret = make_secret();
    let (url, captured) = spawn_receiver().await;

    let registry = Registry::new();
    let _service = caudal_health::start(
        registry.clone(),
        HealthConfig {
            no_keyframe_secs: Some(1),
            min_bitrate_kbps: None,
            min_bitrate_for_secs: 1,
            no_audio_secs: None,
            publisher_lost: false,
            publisher_lost_grace_secs: 1,
            min_hold_secs: 1,
            webhooks: vec![url],
            secret: secret.clone(),
            overrides: Vec::new(),
        },
    )
    .unwrap();

    let publisher = registry.publish("cam-1", BufferConfig::default()).unwrap();
    publisher.set_tracks(vec![video_track()]).unwrap();
    publisher.push(Frame { track: TrackId(0), dts: 0, pts: 0, keyframe: true, data: Default::default() }).unwrap();

    // The publisher then stops sending keyframes entirely: no_keyframe_secs
    // (1s) elapses and the watcher's 1s tick must fire exactly one alert.
    wait_for(&captured, 1).await;

    // Recovery: keyframes well under 1s apart, kept up (as a recovered
    // encoder does) until the resolved event arrives -> exactly one
    // resolved. Stopping after 1.2 s used to make the stream unhealthy again
    // one second later, so resolving depended on where the 1 s ticks fell
    // (failed 2 runs in 5).
    let recovering = async {
        let mut ts = 0i64;
        loop {
            tokio::time::sleep(Duration::from_millis(200)).await;
            ts += 18_000; // 200ms at 90kHz
            publisher
                .push(Frame { track: TrackId(0), dts: ts, pts: ts, keyframe: true, data: Default::default() })
                .unwrap();
        }
    };
    tokio::select! {
        _ = recovering => unreachable!(),
        _ = wait_for(&captured, 2) => {}
    }

    // Give any spurious extra delivery a moment to arrive, then check the
    // count stayed at exactly 2 (hysteresis: no flapping).
    tokio::time::sleep(Duration::from_millis(500)).await;
    let events = captured.events.lock().unwrap().clone();
    assert_eq!(events.len(), 2, "exactly one alert and one resolved, no flapping");

    let webhook = standardwebhooks::Webhook::new(&secret).unwrap();
    let mut bodies = Vec::new();
    for (headers, body) in &events {
        webhook.verify(body, headers).expect("signature must verify");
        bodies.push(serde_json::from_slice::<serde_json::Value>(body).unwrap());
    }

    assert_eq!(bodies[0]["event"], "alert");
    assert_eq!(bodies[0]["rule"], "no_keyframe");
    assert_eq!(bodies[0]["stream"], "cam-1");
    assert!(bodies[0]["text"].as_str().unwrap().contains("ALERT"));

    assert_eq!(bodies[1]["event"], "resolved");
    assert_eq!(bodies[1]["rule"], "no_keyframe");
    assert_eq!(bodies[1]["stream"], "cam-1");
    assert!(bodies[1]["text"].as_str().unwrap().contains("RESOLVED"));
}
