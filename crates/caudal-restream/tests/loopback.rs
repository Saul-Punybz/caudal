//! Integration tests, no external processes.
//!
//! `restreams_into_a_real_rtmp_ingest`: starts `caudal_rtmp::serve` (the
//! project's own RTMP ingest) as the restream target, publishes a source
//! stream through `caudal_core::Registry` directly, and asserts the target
//! server sees the same tracks and frame timestamps, and that the status
//! API never leaks the stream key.
//!
//! `reconnects_after_the_target_drops`: a minimal test-only RTMP server
//! built on `rml_rtmp::sessions::ServerSession` (the same crate the client
//! side uses) so the test can kill and resurrect one specific connection
//! deterministically, to check the waiting/connecting/live/retrying state
//! machine in `crate::push`.

use std::collections::VecDeque;
use std::net::TcpListener as StdTcpListener;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use bytes::Bytes;
use caudal_core::{AudioParams, BufferConfig, Codec, Event, Registry, StartAt, TrackId, TrackInfo, VideoParams};
use caudal_restream::{RestreamConfig, RestreamHandle, RestreamTarget};
use rml_rtmp::handshake::{Handshake, HandshakeProcessResult, PeerType};
use rml_rtmp::sessions::{ServerSession, ServerSessionConfig, ServerSessionEvent, ServerSessionResult};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tower::ServiceExt;

const SECRET_KEY: &str = "topsecret-stream-key-should-never-leak";

fn free_port() -> u16 {
    StdTcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

async fn wait_for<T>(timeout: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for condition");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn status_json(router: &Router) -> serde_json::Value {
    let req = Request::builder().uri("/api/v1/restreams").body(Body::empty()).unwrap();
    let res = router.clone().oneshot(req).await.unwrap();
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    assert!(
        !body.windows(SECRET_KEY.len()).any(|w| w == SECRET_KEY.as_bytes()),
        "stream key leaked into the status API"
    );
    serde_json::from_slice(&body).unwrap()
}

fn h264_track() -> TrackInfo {
    TrackInfo {
        id: TrackId(0),
        codec: Codec::H264,
        timescale: 90_000,
        init: Bytes::from_static(b"fake-avcC"),
        lang: None,
        video: Some(VideoParams { width: 1280, height: 720, fps: None }),
        audio: None,
    }
}

fn aac_track() -> TrackInfo {
    TrackInfo {
        id: TrackId(1),
        codec: Codec::Aac,
        timescale: 44_100,
        // A real AudioSpecificConfig (AAC-LC, 44.1kHz, stereo), needed
        // because the receiving ingest rejects an audio track it can't
        // parse an ASC from.
        init: Bytes::from_static(&[0x12, 0x10]),
        lang: None,
        video: None,
        audio: Some(AudioParams { sample_rate: 44_100, channels: 2 }),
    }
}

/// 40ms per frame at 90kHz: converts to a whole number of RTMP
/// milliseconds and back with no rounding loss.
fn video_frame(n: i64) -> caudal_core::Frame {
    let ts = n * 3600;
    caudal_core::Frame { track: TrackId(0), dts: ts, pts: ts, keyframe: true, data: Bytes::from(vec![0xAAu8; 16]) }
}

/// 50ms per frame at 44.1kHz: same exactness property for audio.
fn audio_frame(n: i64) -> caudal_core::Frame {
    let ts = n * 2205;
    caudal_core::Frame { track: TrackId(1), dts: ts, pts: ts, keyframe: true, data: Bytes::from(vec![0xBBu8; 8]) }
}

#[tokio::test]
async fn restreams_into_a_real_rtmp_ingest() {
    // The target: Caudal's own RTMP ingest, standing in for "any RTMP ingest".
    let dst_port = free_port();
    let dst_registry = Registry::new();
    {
        let reg = dst_registry.clone();
        tokio::spawn(async move {
            let cfg = caudal_rtmp::RtmpConfig {
                bind: format!("127.0.0.1:{dst_port}").parse().unwrap(),
                app: "live".to_owned(),
                buffer: BufferConfig::default(),
            };
            let _ = caudal_rtmp::serve(cfg, reg).await;
        });
    }
    wait_for(Duration::from_secs(2), || std::net::TcpStream::connect(("127.0.0.1", dst_port)).ok().map(drop)).await;

    // The source: a plain Registry, published directly (no real RTMP
    // ingest needed on this side).
    let src_registry = Registry::new();
    let target =
        RestreamTarget { stream: "src".to_owned(), url: format!("rtmp://127.0.0.1:{dst_port}/live/{SECRET_KEY}") };
    let handle: RestreamHandle = caudal_restream::start(src_registry.clone(), RestreamConfig { targets: vec![target] });
    let router = caudal_restream::router(handle);

    let waiting = status_json(&router).await;
    assert_eq!(waiting[0]["state"], "waiting");
    assert_eq!(waiting[0]["stream"], "src");
    assert!(waiting[0]["target"].as_str().unwrap().ends_with("/****"));

    let publisher = src_registry.publish("src", BufferConfig::default()).unwrap();
    publisher.set_tracks(vec![h264_track(), aac_track()]).unwrap();

    // Wait for the push task to connect and get accepted before feeding it
    // frames, so its `StartAt::LiveEdge` subscribe lands before frame 0
    // (otherwise, with every test frame a keyframe, it could join on a
    // later one and "miss" the earlier frames by design, not by bug).
    wait_for_status_state(&router, "live", Duration::from_secs(5)).await;

    for n in 0..6i64 {
        publisher.push(video_frame(n)).unwrap();
        publisher.push(audio_frame(n)).unwrap();
    }

    // caudal-rtmp names the stream after the RTMP stream key.
    let dst_stream = wait_for(Duration::from_secs(5), || dst_registry.get(SECRET_KEY)).await;
    wait_for(Duration::from_secs(5), || {
        let stats = dst_stream.stats();
        (stats.frames_in >= 10).then_some(())
    })
    .await;

    let tracks = dst_stream.tracks();
    assert_eq!(tracks.len(), 2, "expected one video and one audio track, got {tracks:?}");
    assert!(tracks.iter().any(|t| t.codec == Codec::H264));
    assert!(tracks.iter().any(|t| t.codec == Codec::Aac));

    // The target ingest drops media until it has seen both sequence
    // headers, so the first frames may be missing by design. What arrives
    // must be the exact tail of what was sent: no gaps, no rewritten
    // timestamps (a frozen-timestamp bug in the RTMP chunk reader showed up
    // here first; see vendor/README.md).
    let mut sub = dst_stream.subscribe(StartAt::Oldest);
    let mut video_ts = Vec::new();
    let mut audio_ts = Vec::new();
    while video_ts.last() != Some(&(5 * 3600)) || audio_ts.last() != Some(&(5 * 2205)) {
        match tokio::time::timeout(Duration::from_secs(5), sub.recv()).await.expect("timed out reading dst frames") {
            Event::Frame(f) if f.track == TrackId(0) => video_ts.push(f.dts),
            Event::Frame(f) if f.track == TrackId(1) => audio_ts.push(f.dts),
            Event::Frame(_) | Event::TracksChanged | Event::Lagged { .. } | Event::Cue(_) => {}
            Event::End => panic!("dst ended before all frames arrived"),
        }
    }

    let expected_video: Vec<i64> = (0..6).map(|n| n * 3600).collect();
    let expected_audio: Vec<i64> = (0..6).map(|n| n * 2205).collect();
    assert!(
        video_ts.len() >= 5 && expected_video.ends_with(&video_ts),
        "video timestamps did not round-trip: {video_ts:?}"
    );
    assert!(
        audio_ts.len() >= 5 && expected_audio.ends_with(&audio_ts),
        "audio timestamps did not round-trip: {audio_ts:?}"
    );

    let live_status = status_json(&router).await;
    assert_eq!(live_status[0]["state"], "live");
    assert!(live_status[0]["bytes_sent"].as_u64().unwrap() > 0);
}

/// Polls the status API until the one configured target reports `state`.
async fn wait_for_status_state(router: &Router, state: &str, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let body = status_json(router).await;
        if body[0]["state"] == state {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "target never reached {state}: {body:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// --- A minimal test-only RTMP server, built on the same `rml_rtmp` crate
// the client side uses, so the test can kill one specific connection on
// demand and bring the target back without rebinding the port. ---

async fn handle_results(
    session: &mut ServerSession,
    socket: &mut TcpStream,
    results: Vec<ServerSessionResult>,
) -> bool {
    let mut queue: VecDeque<ServerSessionResult> = results.into();
    while let Some(r) = queue.pop_front() {
        match r {
            ServerSessionResult::OutboundResponse(p) => {
                if socket.write_all(&p.bytes).await.is_err() {
                    return false;
                }
            }
            ServerSessionResult::RaisedEvent(ServerSessionEvent::ConnectionRequested { request_id, .. })
            | ServerSessionResult::RaisedEvent(ServerSessionEvent::PublishStreamRequested { request_id, .. }) => {
                match session.accept_request(request_id) {
                    Ok(more) => {
                        for item in more.into_iter().rev() {
                            queue.push_front(item);
                        }
                    }
                    Err(_) => return false,
                }
            }
            _ => {}
        }
    }
    true
}

async fn serve_one_connection(mut socket: TcpStream) {
    let mut handshake = Handshake::new(PeerType::Server);
    let mut buf = [0u8; 4096];
    let leftover;
    loop {
        let n = match socket.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        match handshake.process_bytes(&buf[..n]) {
            Ok(HandshakeProcessResult::InProgress { response_bytes }) => {
                if !response_bytes.is_empty() && socket.write_all(&response_bytes).await.is_err() {
                    return;
                }
            }
            Ok(HandshakeProcessResult::Completed { response_bytes, remaining_bytes }) => {
                if !response_bytes.is_empty() && socket.write_all(&response_bytes).await.is_err() {
                    return;
                }
                leftover = remaining_bytes;
                break;
            }
            Err(_) => return,
        }
    }

    let Ok((mut session, initial)) = ServerSession::new(ServerSessionConfig::new()) else { return };
    if !handle_results(&mut session, &mut socket, initial).await {
        return;
    }
    if !leftover.is_empty() {
        match session.handle_input(&leftover) {
            Ok(results) => {
                if !handle_results(&mut session, &mut socket, results).await {
                    return;
                }
            }
            Err(_) => return,
        }
    }

    loop {
        let n = match socket.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        match session.handle_input(&buf[..n]) {
            Ok(results) => {
                if !handle_results(&mut session, &mut socket, results).await {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

async fn run_fake_target(listener: TcpListener, kill: Arc<Notify>) {
    loop {
        let Ok((socket, _)) = listener.accept().await else { return };
        let kill = kill.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = kill.notified() => {}
                _ = serve_one_connection(socket) => {}
            }
        });
    }
}

#[tokio::test]
async fn reconnects_after_the_target_drops() {
    let port = free_port();
    let listener = TcpListener::bind(format!("127.0.0.1:{port}")).await.unwrap();
    let kill = Arc::new(Notify::new());
    tokio::spawn(run_fake_target(listener, kill.clone()));

    let src_registry = Registry::new();
    let target = RestreamTarget { stream: "src".to_owned(), url: format!("rtmp://127.0.0.1:{port}/live/{SECRET_KEY}") };
    let handle = caudal_restream::start(src_registry.clone(), RestreamConfig { targets: vec![target] });
    let router = caudal_restream::router(handle);

    let publisher = src_registry.publish("src", BufferConfig::default()).unwrap();
    publisher.set_tracks(vec![h264_track()]).unwrap();

    wait_for_status_state(&router, "live", Duration::from_secs(5)).await;

    kill.notify_one();
    wait_for_status_state(&router, "retrying", Duration::from_secs(5)).await;
    wait_for_status_state(&router, "live", Duration::from_secs(5)).await;

    drop(publisher);
}
