//! The runner against a real `Registry`, on a paused clock: simulated
//! encoders push frames, a viewer reads the public stream.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use bytes::Bytes;
use caudal_core::{
    AudioParams, BufferConfig, Codec, Event, Frame, Publisher, Registry, StartAt, Subscriber, TrackId, TrackInfo,
    VideoParams,
};
use tower::ServiceExt;

use super::*;

const SWITCH_AFTER: Duration = Duration::from_millis(500);
const HOLD: Duration = Duration::from_secs(2);
const FRAME: Duration = Duration::from_millis(33);

fn video(width: u32) -> TrackInfo {
    TrackInfo {
        id: TrackId(0),
        codec: Codec::H264,
        timescale: 90_000,
        init: Bytes::from(format!("avcC-{width}")),
        lang: None,
        video: Some(VideoParams { width, height: width * 9 / 16, fps: None }),
        audio: None,
    }
}

fn audio() -> TrackInfo {
    TrackInfo {
        id: TrackId(1),
        codec: Codec::Aac,
        timescale: 48_000,
        init: Bytes::from_static(&[0x11, 0x90]),
        lang: None,
        video: None,
        audio: Some(AudioParams { sample_rate: 48_000, channels: 2 }),
    }
}

fn failover(sources: &[&str]) -> Failover {
    Failover {
        stream: "main".into(),
        sources: sources.iter().map(|s| Source::parse(s).unwrap()).collect(),
        switch_after: SWITCH_AFTER,
        switch_back_after: HOLD,
    }
}

fn start_one(registry: &Arc<Registry>, f: Failover) -> FailoverHandle {
    start(registry.clone(), FailoverConfig { entries: vec![f], buffer: BufferConfig::default() })
}

/// A simulated encoder: 30 fps video with a keyframe every second, plus
/// AAC; payloads carry `tag` so the viewer can tell sources apart. Its
/// clock starts at `v0` ticks.
struct Enc {
    p: Publisher,
    tag: &'static str,
    n: i64,
    v0: i64,
}

impl Enc {
    fn new(registry: &Arc<Registry>, name: &str, tag: &'static str, width: u32, v0: i64) -> Self {
        let p = registry.publish(name, BufferConfig::default()).unwrap();
        p.set_tracks(vec![video(width), audio()]).unwrap();
        Self { p, tag, n: 0, v0 }
    }

    fn frame(&mut self) {
        let data = Bytes::from(self.tag);
        let dts = self.v0 + self.n * 3000;
        let key = self.n % 30 == 0;
        self.p.push(Frame { track: TrackId(0), dts, pts: dts, keyframe: key, data: data.clone() }).unwrap();
        let a = self.v0 * 48 / 90 + self.n * 1600;
        self.p.push(Frame { track: TrackId(1), dts: a, pts: a, keyframe: true, data }).unwrap();
        self.n += 1;
    }
}

/// Lets `ms` of virtual time pass, `live` encoders pushing a frame each
/// 33 ms.
async fn run(live: &mut [&mut Enc], ms: u64) {
    for _ in 0..ms / 33 {
        for e in live.iter_mut() {
            e.frame();
        }
        tokio::time::sleep(FRAME).await;
    }
}

/// What a viewer of the public stream got.
struct Viewer {
    sub: Subscriber,
    /// (event index, frame) for frames; TracksChanged positions apart.
    frames: Vec<Frame>,
    tracks_changed: Vec<usize>,
}

impl Viewer {
    fn join(registry: &Arc<Registry>) -> Self {
        let s = registry.get("main").expect("public stream on air");
        Self { sub: s.subscribe_internal(StartAt::Oldest), frames: Vec::new(), tracks_changed: Vec::new() }
    }

    fn drain(&mut self) {
        while let Some(ev) = self.sub.try_recv() {
            match ev {
                Event::Frame(f) => self.frames.push((*f).clone()),
                Event::TracksChanged => self.tracks_changed.push(self.frames.len()),
                Event::End => panic!("the public stream ended"),
                _ => {}
            }
        }
    }

    fn tags(&self) -> Vec<(&[u8], bool)> {
        self.frames.iter().filter(|f| f.track == TrackId(0)).map(|f| (&f.data[..], f.keyframe)).collect()
    }

    /// Index (among video frames) of every source change, with the frame.
    fn seams(&self) -> Vec<(usize, &Frame)> {
        let video: Vec<&Frame> = self.frames.iter().filter(|f| f.track == TrackId(0)).collect();
        (1..video.len()).filter(|&i| video[i].data != video[i - 1].data).map(|i| (i, video[i])).collect()
    }

    fn assert_monotonic(&self) {
        let mut last: HashMap<TrackId, i64> = HashMap::new();
        for f in &self.frames {
            if let Some(&p) = last.get(&f.track) {
                assert!(f.dts > p, "track {:?} went back {p} -> {}", f.track, f.dts);
            }
            last.insert(f.track, f.dts);
        }
    }
}

fn reasons(h: &FailoverHandle) -> Vec<&'static str> {
    h.status()[0].history.iter().map(|e| e.reason).collect()
}

#[tokio::test(start_paused = true)]
async fn switches_on_silence_and_back_after_the_hold() {
    let registry = Registry::new();
    let h = start_one(&registry, failover(&["cam", "cam-backup"]));
    // Different clocks: the backup's is far behind the primary's.
    let mut cam = Enc::new(&registry, "cam", "P", 1280, 90_000_000);
    let mut bak = Enc::new(&registry, "cam-backup", "B", 1280, 0);
    run(&mut [&mut cam, &mut bak], 1000).await;
    let mut v = Viewer::join(&registry);
    assert_eq!(h.status()[0].active.as_deref(), Some("cam"));

    // The primary goes silent (still connected).
    run(&mut [&mut bak], 300).await;
    assert_eq!(h.status()[0].active.as_deref(), Some("cam"), "not before switch_after");
    run(&mut [&mut bak], 400).await;
    let st = &h.status()[0];
    assert_eq!(st.active.as_deref(), Some("cam-backup"), "switched within switch_after + a tick");
    assert_eq!(st.last_switch.as_ref().unwrap().reason, "silent");
    assert!(!st.sources[0].healthy);

    // The primary returns; the backup stays on for the hold.
    run(&mut [&mut cam, &mut bak], 1500).await;
    assert_eq!(h.status()[0].active.as_deref(), Some("cam-backup"), "held");
    run(&mut [&mut cam, &mut bak], 1000).await;
    assert_eq!(h.status()[0].active.as_deref(), Some("cam"), "back after the hold");
    run(&mut [&mut cam, &mut bak], 500).await;

    assert_eq!(reasons(&h), ["start", "silent", "recovered"]);
    v.drain();
    v.assert_monotonic();
    let seams = v.seams();
    assert_eq!(seams.len(), 2, "P -> B -> P: {:?}", v.tags());
    for (_, f) in &seams {
        assert!(f.keyframe, "every switch starts on a keyframe");
    }
    assert_eq!(&seams[0].1.data[..], b"B");
    assert_eq!(&seams[1].1.data[..], b"P");
    assert_eq!(v.tracks_changed, [0], "identical track lists: no TracksChanged after joining");
    // The seam continues the timeline instead of jumping.
    let video: Vec<&Frame> = v.frames.iter().filter(|f| f.track == TrackId(0)).collect();
    let (i, f) = seams[0];
    let step = f.dts - video[i - 1].dts;
    assert_eq!(step, 3000, "one frame after the last primary frame");
}

#[tokio::test(start_paused = true)]
async fn a_codec_change_reaches_viewers_as_tracks_changed() {
    let registry = Registry::new();
    let _h = start_one(&registry, failover(&["cam", "slate"]));
    let mut cam = Enc::new(&registry, "cam", "P", 1280, 0);
    let mut slate = Enc::new(&registry, "slate", "S", 640, 0);
    run(&mut [&mut cam, &mut slate], 1000).await;
    let mut v = Viewer::join(&registry);
    v.drain();
    let before = v.tracks_changed.len();
    drop(cam);
    run(&mut [&mut slate], 1500).await;
    v.drain();
    assert_eq!(v.tracks_changed.len(), before + 1, "one TracksChanged at the switch");
    let at = *v.tracks_changed.last().unwrap();
    let first = &v.frames[at];
    assert!(first.keyframe && first.track == TrackId(0) && &first.data[..] == b"S", "{first:?}");
    let tracks = v.sub.tracks();
    assert_eq!(tracks[0].video.unwrap().width, 640);
    v.assert_monotonic();
}

#[tokio::test(start_paused = true)]
async fn a_quick_republish_of_the_active_source_is_not_a_switch() {
    let registry = Registry::new();
    let h = start_one(&registry, failover(&["cam", "cam-backup"]));
    let mut cam = Enc::new(&registry, "cam", "P", 1280, 0);
    let mut bak = Enc::new(&registry, "cam-backup", "B", 1280, 0);
    run(&mut [&mut cam, &mut bak], 1000).await;
    let mut v = Viewer::join(&registry);
    drop(cam);
    run(&mut [&mut bak], 200).await;
    // The encoder is back, clock restarted at zero.
    let mut cam = Enc::new(&registry, "cam", "P", 1280, 0);
    run(&mut [&mut cam, &mut bak], 1000).await;
    assert_eq!(reasons(&h), ["start"]);
    assert_eq!(h.status()[0].active.as_deref(), Some("cam"));
    v.drain();
    v.assert_monotonic();
    assert!(v.tags().iter().all(|(t, _)| *t == b"P"));
    // A publisher that stays gone is failed over from.
    drop(cam);
    run(&mut [&mut bak], 1000).await;
    assert_eq!(reasons(&h), ["start", "silent"]);
}

#[tokio::test(start_paused = true)]
async fn manual_switch_pins_a_source_until_automatic() {
    let registry = Registry::new();
    let h = start_one(&registry, failover(&["cam", "cam-backup", "spare"]));
    let app = router(h.clone());
    let mut cam = Enc::new(&registry, "cam", "P", 1280, 0);
    let mut bak = Enc::new(&registry, "cam-backup", "B", 1280, 0);
    run(&mut [&mut cam, &mut bak], 1000).await;
    let mut v = Viewer::join(&registry);

    let post = |path: &str, body: &str| {
        Request::post(path).header("content-type", "application/json").body(Body::from(body.to_owned())).unwrap()
    };
    let status = |r: Request<Body>| {
        let app = app.clone();
        async move { app.oneshot(r).await.unwrap().status() }
    };
    let sw = "/api/v1/failover/main/switch";
    assert_eq!(status(post("/api/v1/failover/nope/switch", r#"{"source":"cam"}"#)).await, StatusCode::NOT_FOUND);
    assert_eq!(status(post(sw, r#"{"source":"other"}"#)).await, StatusCode::BAD_REQUEST);
    assert_eq!(status(post(sw, "not json")).await, StatusCode::BAD_REQUEST);
    assert_eq!(status(post(sw, r#"{"source":"spare"}"#)).await, StatusCode::CONFLICT, "spare is not published");
    assert_eq!(status(post(sw, r#"{"source":"cam-backup"}"#)).await, StatusCode::NO_CONTENT);
    let st = &h.status()[0];
    assert_eq!(st.active.as_deref(), Some("cam-backup"));
    assert_eq!(st.preferred.as_deref(), Some("cam-backup"));

    // Pinned: the healthy primary does not take over after the hold.
    run(&mut [&mut cam, &mut bak], 3000).await;
    assert_eq!(h.status()[0].active.as_deref(), Some("cam-backup"));

    // Back to automatic: the primary has been healthy all along.
    assert_eq!(status(post(sw, r#"{"source":null}"#)).await, StatusCode::NO_CONTENT);
    run(&mut [&mut cam, &mut bak], 300).await;
    assert_eq!(h.status()[0].active.as_deref(), Some("cam"));
    assert_eq!(reasons(&h), ["start", "manual", "recovered"]);

    v.drain();
    v.assert_monotonic();
    assert!(v.seams().iter().all(|(_, f)| f.keyframe));

    let res = app.clone().oneshot(Request::get("/api/v1/failover").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json[0]["stream"], "main");
    assert_eq!(json[0]["active"], "cam");
    assert_eq!(json[0]["switches"], 3);
    assert_eq!(json[0]["sources"][2]["live"], false);
}

#[tokio::test(start_paused = true)]
async fn switch_events_are_broadcast() {
    let registry = Registry::new();
    let h = start_one(&registry, failover(&["cam", "cam-backup"]));
    let mut events = h.subscribe_switches();
    let mut cam = Enc::new(&registry, "cam", "P", 1280, 0);
    let mut bak = Enc::new(&registry, "cam-backup", "B", 1280, 0);
    run(&mut [&mut cam, &mut bak], 500).await;
    drop(cam);
    run(&mut [&mut bak], 1000).await;
    let a = events.try_recv().unwrap();
    assert_eq!((a.from, a.to.as_str(), a.reason), (None, "cam", "start"));
    let b = events.try_recv().unwrap();
    assert_eq!((b.from.as_deref(), b.to.as_str(), b.reason), (Some("cam"), "cam-backup", "silent"));
}

#[tokio::test(start_paused = true)]
async fn reload_keeps_unchanged_entries_running() {
    let registry = Registry::new();
    let h = start_one(&registry, failover(&["cam", "cam-backup"]));
    let mut cam = Enc::new(&registry, "cam", "P", 1280, 0);
    run(&mut [&mut cam], 500).await;
    let stream = registry.get("main").unwrap();
    h.reload(FailoverConfig { entries: vec![failover(&["cam", "cam-backup"])], buffer: BufferConfig::default() });
    run(&mut [&mut cam], 300).await;
    assert!(Arc::ptr_eq(&stream, &registry.get("main").unwrap()), "same publish");
    h.reload(FailoverConfig { entries: Vec::new(), buffer: BufferConfig::default() });
    run(&mut [&mut cam], 100).await;
    assert!(registry.get("main").is_none(), "dropped entry ends its stream");
    assert!(h.status().is_empty());
}

#[test]
fn config_validation() {
    assert!(Source::parse("file:").is_err());
    assert!(Source::parse("a b").is_err());
    assert_eq!(Source::parse("file:/x.mp4").unwrap(), Source::File("/x.mp4".into()));
    assert!(failover(&["cam", "file:/slate.mp4"]).validate().is_ok());
    assert!(failover(&[]).validate().is_err());
    assert!(failover(&["main"]).validate().is_err(), "itself");
    assert!(failover(&["cam", "cam"]).validate().is_err(), "twice");
    let mut f = failover(&["cam"]);
    f.switch_after = Duration::ZERO;
    assert!(f.validate().is_err());
}
