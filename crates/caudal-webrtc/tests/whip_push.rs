//! WHIP push out, Caudal to Caudal: one registry's `[[webrtc.push]]` sends
//! a live stream into a second Caudal's WHIP ingest router over real UDP
//! on localhost. The receiving router is wrapped in a layer that records
//! every request (method, path, `Authorization`), so the test sees the
//! offer `POST`, the bearer token and the final `DELETE` on the wire.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use bytes::Bytes;
use caudal_core::{
    Access, AudioParams, BufferConfig, Codec, Denied, Event, Frame, Gate, GateFuture, Registry, StartAt, TrackId,
    TrackInfo, VideoParams,
};
use caudal_webrtc::{WebRtcConfig, WhipPush, router, start_pushes};

#[derive(Debug, Clone)]
struct Hit {
    method: String,
    path: String,
    auth: Option<String>,
    at: Instant,
}

#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<Hit>>>);

impl Log {
    fn hits(&self) -> Vec<Hit> {
        self.0.lock().unwrap().clone()
    }
}

async fn record(State(log): State<Log>, req: Request, next: Next) -> Response {
    let auth = req.headers().get("authorization").and_then(|v| v.to_str().ok()).map(str::to_owned);
    log.0.lock().unwrap().push(Hit {
        method: req.method().to_string(),
        path: req.uri().path().to_owned(),
        auth,
        at: Instant::now(),
    });
    next.run(req).await
}

/// Publishing needs `Bearer s3cret`.
struct NeedsToken;

impl Gate for NeedsToken {
    fn check<'a>(&'a self, a: Access, _: &'a str, t: Option<&'a str>, _: Option<std::net::IpAddr>) -> GateFuture<'a> {
        Box::pin(async move {
            match (a, t) {
                (Access::Play, _) | (_, Some("s3cret")) => Ok(()),
                (_, None) => Err(Denied::Missing),
                _ => Err(Denied::Refused("bad token".into())),
            }
        })
    }
}

/// A second Caudal's WHIP/WHEP router on free ports; its HTTP base URL.
async fn receiver(registry: Arc<Registry>, log: Log) -> String {
    let app = router(
        registry,
        WebRtcConfig {
            udp_bind: "127.0.0.1:0".parse().unwrap(),
            public_ips: vec![],
            buffer: BufferConfig::default(),
            threads: 1,
        },
        Vec::new(),
    )
    .layer(axum::middleware::from_fn_with_state(log, record));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr.to_string()
}

/// The fixture as AVCC access units (split on AUD), plus its SPS and PPS,
/// with SPS/PPS taken out of the frames (the sender must add them from
/// the avcC).
fn fixture() -> (Vec<(bool, Bytes)>, Vec<u8>, Vec<u8>) {
    let raw = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/clip.h264")).unwrap();
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 2 < raw.len() {
        if raw[i] == 0 && raw[i + 1] == 0 && raw[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let (mut sps, mut pps) = (Vec::new(), Vec::new());
    let mut aus: Vec<(bool, Bytes)> = Vec::new();
    let (mut cur, mut key) = (Vec::new(), false);
    for (k, &s) in starts.iter().enumerate() {
        let mut e = starts.get(k + 1).map_or(raw.len(), |&n| n - 3);
        while e > s && raw[e - 1] == 0 {
            e -= 1;
        }
        let nal = &raw[s..e];
        match nal[0] & 0x1f {
            9 => {
                if !cur.is_empty() {
                    aus.push((key, Bytes::from(std::mem::take(&mut cur))));
                }
                key = false;
                continue;
            }
            7 => {
                sps = nal.to_vec();
                continue;
            }
            8 => {
                pps = nal.to_vec();
                continue;
            }
            5 => key = true,
            _ => {}
        }
        cur.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        cur.extend_from_slice(nal);
    }
    if !cur.is_empty() {
        aus.push((key, Bytes::from(cur)));
    }
    (aus, sps, pps)
}

fn avcc_record(sps: &[u8], pps: &[u8]) -> Bytes {
    let mut r = vec![1, sps[1], sps[2], sps[3], 0xff, 0xe1];
    r.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    r.extend_from_slice(sps);
    r.push(1);
    r.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    r.extend_from_slice(pps);
    Bytes::from(r)
}

/// Publishes `name` on `registry` with H.264 (+ Opus when `opus`) and
/// feeds the clip in a loop at 30 fps. Aborting the task ends the stream.
fn publish_clip(registry: &Arc<Registry>, name: &str, opus: bool) -> tokio::task::JoinHandle<()> {
    let (aus, sps, pps) = fixture();
    let publisher = registry.publish(name, BufferConfig::default()).unwrap();
    let mut tracks = vec![TrackInfo {
        id: TrackId(0),
        codec: Codec::H264,
        timescale: 90_000,
        init: avcc_record(&sps, &pps),
        lang: None,
        video: Some(VideoParams { width: 320, height: 240, fps: Some(30.0) }),
        audio: None,
    }];
    if opus {
        tracks.push(TrackInfo {
            id: TrackId(1),
            codec: Codec::Opus,
            timescale: 48_000,
            init: Bytes::from_static(b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00"),
            lang: None,
            video: None,
            audio: Some(AudioParams { sample_rate: 48_000, channels: 2 }),
        });
    }
    publisher.set_tracks(tracks).unwrap();
    tokio::spawn(async move {
        let mut n: i64 = 0;
        loop {
            for (key, data) in &aus {
                let v = Frame { track: TrackId(0), dts: n * 3000, pts: n * 3000, keyframe: *key, data: data.clone() };
                if publisher.push(v).is_err() {
                    return;
                }
                if opus {
                    // A silent 20 ms Opus packet per video frame.
                    let a = Frame {
                        track: TrackId(1),
                        dts: n * 1600,
                        pts: n * 1600,
                        keyframe: true,
                        data: Bytes::from_static(&[0xf8, 0xff, 0xfe]),
                    };
                    if publisher.push(a).is_err() {
                        return;
                    }
                }
                n += 1;
                tokio::time::sleep(Duration::from_millis(33)).await;
            }
        }
    })
}

async fn wait_for<T>(what: &str, timeout: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let t0 = Instant::now();
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(t0.elapsed() < timeout, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// (video frames, video keyframes, audio frames) arriving within `window`.
async fn count_frames(stream: &Arc<caudal_core::Stream>, window: Duration) -> (usize, usize, usize) {
    let tracks = stream.tracks();
    let video = tracks.iter().find(|t| t.codec == Codec::H264).map(|t| t.id);
    let audio = tracks.iter().find(|t| t.codec == Codec::Opus).map(|t| t.id);
    let mut sub = stream.subscribe_internal(StartAt::LiveEdge);
    let (mut v, mut k, mut a) = (0, 0, 0);
    let end = tokio::time::Instant::now() + window;
    while let Ok(ev) = tokio::time::timeout_at(end, sub.recv()).await {
        match ev {
            Event::Frame(f) if Some(f.track) == video => {
                v += 1;
                k += usize::from(f.keyframe);
            }
            Event::Frame(f) if Some(f.track) == audio => a += 1,
            Event::End => break,
            _ => {}
        }
    }
    (v, k, a)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn caudal_pushes_to_caudal_over_whip() {
    let source = Registry::new();
    let sink = Registry::new();
    sink.set_gate(Arc::new(NeedsToken));
    let log = Log::default();
    let addr = receiver(sink.clone(), log.clone()).await;

    let pushes = start_pushes(
        source.clone(),
        vec![WhipPush {
            stream: "live".into(),
            url: format!("whip+http://{addr}/whip/cam"),
            token: Some("s3cret".into()),
        }],
    );
    // Published after the push started: it waits for the stream.
    let feeder = publish_clip(&source, "live", true);

    let cam = wait_for("the pushed stream with H.264 and Opus", Duration::from_secs(20), || {
        let s = sink.get("cam")?;
        let t = s.tracks();
        (t.iter().any(|t| t.codec == Codec::H264) && t.iter().any(|t| t.codec == Codec::Opus)).then_some(s)
    })
    .await;
    let (v, k, a) = count_frames(&cam, Duration::from_secs(3)).await;
    assert!(v >= 45, "video keeps arriving: {v} frames in 3 s");
    assert!(k >= 1, "a keyframe came through");
    assert!(a >= 30, "audio keeps arriving: {a} frames in 3 s");
    let (v2, _, a2) = count_frames(&cam, Duration::from_secs(2)).await;
    assert!(v2 >= 30 && a2 >= 20, "still flowing: {v2} video, {a2} audio frames in 2 s");

    let hits = log.hits();
    let post = hits.iter().find(|h| h.method == "POST").expect("an offer was posted");
    assert_eq!(post.path, "/whip/cam");
    assert_eq!(post.auth.as_deref(), Some("Bearer s3cret"));
    assert!(!hits.iter().any(|h| h.method == "DELETE"), "no DELETE while live");

    // The source ends: the push DELETEs its resource and the sink's stream ends.
    feeder.abort();
    let del =
        wait_for("the DELETE", Duration::from_secs(10), || log.hits().into_iter().find(|h| h.method == "DELETE")).await;
    assert!(del.path.starts_with("/whip/cam/"), "{}", del.path);
    assert_eq!(del.auth.as_deref(), Some("Bearer s3cret"));
    wait_for("the pushed stream to end", Duration::from_secs(5), || cam.is_ended().then_some(())).await;
    drop(pushes);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removing_the_push_deletes_the_resource() {
    let source = Registry::new();
    let sink = Registry::new();
    let log = Log::default();
    let addr = receiver(sink.clone(), log.clone()).await;
    let _feeder = publish_clip(&source, "v", false);
    let pushes = start_pushes(
        source.clone(),
        vec![WhipPush { stream: "v".into(), url: format!("whip+http://{addr}/whip/v"), token: None }],
    );
    let v = wait_for("the pushed video-only stream", Duration::from_secs(20), || {
        let s = sink.get("v")?;
        s.tracks().iter().any(|t| t.codec == Codec::H264).then_some(s)
    })
    .await;
    assert!(count_frames(&v, Duration::from_secs(2)).await.0 >= 30, "video arrives");
    assert!(log.hits().iter().all(|h| h.auth.is_none()), "no token, no Authorization");

    // A reload that drops the push: the source is still live, yet the
    // remote resource goes away.
    pushes.reload(&source, vec![]);
    assert!(pushes.targets().is_empty());
    let del =
        wait_for("the DELETE", Duration::from_secs(10), || log.hits().into_iter().find(|h| h.method == "DELETE")).await;
    assert!(del.path.starts_with("/whip/v/"), "{}", del.path);
    wait_for("the pushed stream to end", Duration::from_secs(5), || v.is_ended().then_some(())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_endpoint_is_retried_with_backoff() {
    let log = Log::default();
    let app = axum::Router::new()
        .route("/whip/x", axum::routing::post(|| async { (axum::http::StatusCode::SERVICE_UNAVAILABLE, "busy") }))
        .layer(axum::middleware::from_fn_with_state(log.clone(), record));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let source = Registry::new();
    let _feeder = publish_clip(&source, "x", false);
    let _pushes = start_pushes(
        source.clone(),
        vec![WhipPush { stream: "x".into(), url: format!("whip+http://{addr}/whip/x"), token: Some("t".into()) }],
    );
    // Attempts at about 0 s, 1 s, 3 s, 7 s: three within 4.5 s.
    tokio::time::sleep(Duration::from_millis(4500)).await;
    let at: Vec<Instant> = log.hits().iter().filter(|h| h.method == "POST").map(|h| h.at).collect();
    assert!((2..=4).contains(&at.len()), "{} attempts in 4.5 s", at.len());
    let gaps: Vec<Duration> = at.windows(2).map(|w| w[1] - w[0]).collect();
    assert!(gaps[0] >= Duration::from_millis(900), "first retry waits: {gaps:?}");
    if gaps.len() > 1 {
        assert!(gaps[1] >= gaps[0] + Duration::from_millis(700), "the wait grows: {gaps:?}");
    }
}
