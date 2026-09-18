use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use caudal_core::*;

const V: TrackId = TrackId(0);
const A: TrackId = TrackId(1);

fn video() -> TrackInfo {
    TrackInfo {
        id: V,
        codec: Codec::H264,
        timescale: 90_000,
        init: Bytes::from_static(b"avcC"),
        lang: None,
        video: Some(VideoParams { width: 1280, height: 720, fps: Some(30.0) }),
        audio: None,
    }
}

fn audio() -> TrackInfo {
    TrackInfo {
        id: A,
        codec: Codec::Aac,
        timescale: 48_000,
        init: Bytes::from_static(b"asc"),
        lang: Some("spa".into()),
        video: None,
        audio: Some(AudioParams { sample_rate: 48_000, channels: 2 }),
    }
}

fn vframe(n: i64, size: usize) -> Frame {
    Frame { track: V, dts: n * 3000, pts: n * 3000, keyframe: n % 30 == 0, data: Bytes::from(vec![0; size]) }
}

fn aframe(n: i64) -> Frame {
    Frame { track: A, dts: n * 1024, pts: n * 1024, keyframe: true, data: Bytes::from_static(&[1; 8]) }
}

fn cfg(window_secs: u64) -> BufferConfig {
    BufferConfig { window: Duration::from_secs(window_secs), max_bytes: usize::MAX }
}

fn next_frame(sub: &mut Subscriber) -> Arc<Frame> {
    loop {
        match sub.try_recv().expect("expected an event") {
            Event::Frame(f) => return f,
            Event::TracksChanged => continue,
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[test]
fn viewer_joins_on_the_newest_keyframe() {
    let reg = Registry::new();
    let publ = reg.publish("live", cfg(50)).unwrap();
    publ.set_tracks(vec![video()]).unwrap();
    for n in 0..45 {
        publ.push(vframe(n, 100)).unwrap();
    }
    let mut sub = reg.subscribe("live", StartAt::LiveEdge).unwrap();
    assert_eq!(sub.try_recv(), Some(Event::TracksChanged));
    let f = next_frame(&mut sub);
    assert!(f.keyframe);
    assert_eq!(f.dts, 30 * 3000);

    let mut dvr = reg.subscribe("live", StartAt::Oldest).unwrap();
    assert_eq!(next_frame(&mut dvr).dts, 0);
}

#[test]
fn window_evicts_whole_gops() {
    let reg = Registry::new();
    let publ = reg.publish("live", cfg(3)).unwrap();
    publ.set_tracks(vec![video()]).unwrap();
    for n in 0..300 {
        publ.push(vframe(n, 100)).unwrap();
    }
    let stats = publ.stream().stats();
    // 3 s of window plus at most one GOP of slack.
    assert!(stats.buffered_micros <= 4_000_000, "{stats:?}");
    assert!(stats.buffered_micros >= 3_000_000, "{stats:?}");
    let mut sub = reg.subscribe("live", StartAt::Oldest).unwrap();
    assert!(next_frame(&mut sub).keyframe);
}

#[test]
fn byte_cap_bounds_memory_regardless_of_window() {
    let reg = Registry::new();
    let publ = reg
        .publish("live", BufferConfig { window: Duration::from_secs(3600), max_bytes: 100_000 })
        .unwrap();
    publ.set_tracks(vec![video()]).unwrap();
    for n in 0..600 {
        publ.push(vframe(n, 1_000)).unwrap();
    }
    let stats = publ.stream().stats();
    assert!(stats.bytes_buffered <= 100_000, "{stats:?}");
    assert_eq!(stats.bytes_in, 600_000);
}

#[test]
fn slow_viewer_is_moved_to_a_keyframe_instead_of_blocking_ingest() {
    let reg = Registry::new();
    let publ = reg.publish("live", cfg(2)).unwrap();
    publ.set_tracks(vec![video()]).unwrap();
    publ.push(vframe(0, 10)).unwrap();
    let mut sub = reg.subscribe("live", StartAt::Oldest).unwrap();
    assert_eq!(sub.try_recv(), Some(Event::TracksChanged));
    assert_eq!(next_frame(&mut sub).dts, 0);

    // The viewer stops reading while ten seconds go by.
    for n in 1..300 {
        publ.push(vframe(n, 10)).unwrap();
    }
    match sub.try_recv() {
        Some(Event::Lagged { skipped }) => assert!(skipped > 0),
        other => panic!("expected Lagged, got {other:?}"),
    }
    assert!(next_frame(&mut sub).keyframe);
}

#[test]
fn ending_drains_then_frees_the_name() {
    let reg = Registry::new();
    let publ = reg.publish("live", cfg(50)).unwrap();
    assert_eq!(reg.publish("live", cfg(50)).err(), Some(PublishError::Busy("live".into())));
    publ.set_tracks(vec![video()]).unwrap();
    publ.push(vframe(0, 10)).unwrap();
    publ.push(vframe(1, 10)).unwrap();
    let mut sub = reg.subscribe("live", StartAt::Oldest).unwrap();
    drop(publ);

    assert!(reg.get("live").is_none());
    assert_eq!(next_frame(&mut sub).dts, 0);
    assert_eq!(next_frame(&mut sub).dts, 3000);
    assert_eq!(sub.try_recv(), Some(Event::End));
    assert!(reg.publish("live", cfg(50)).is_ok());
}

#[test]
fn rejects_bad_names_and_unknown_tracks() {
    let reg = Registry::new();
    assert_eq!(reg.publish("../x", cfg(1)).err(), Some(PublishError::InvalidName));
    let publ = reg.publish("live", cfg(1)).unwrap();
    assert_eq!(publ.push(vframe(0, 1)), Err(PushError::UnknownTrack(V)));
}

#[test]
fn audio_only_streams_join_on_any_frame() {
    let reg = Registry::new();
    let publ = reg.publish("radio", cfg(50)).unwrap();
    publ.set_tracks(vec![audio()]).unwrap();
    for n in 0..10 {
        publ.push(aframe(n)).unwrap();
    }
    let mut sub = reg.subscribe("radio", StartAt::LiveEdge).unwrap();
    assert_eq!(next_frame(&mut sub).dts, 9 * 1024);
}

#[test]
fn audio_does_not_create_join_points_when_there_is_video() {
    let reg = Registry::new();
    let publ = reg.publish("live", cfg(50)).unwrap();
    publ.set_tracks(vec![video(), audio()]).unwrap();
    publ.push(vframe(0, 10)).unwrap();
    publ.push(aframe(0)).unwrap();
    publ.push(vframe(1, 10)).unwrap();
    publ.push(aframe(1)).unwrap();
    let mut sub = reg.subscribe("live", StartAt::LiveEdge).unwrap();
    let f = next_frame(&mut sub);
    assert_eq!((f.track, f.dts), (V, 0));
}

#[test]
fn early_viewer_waits_for_the_first_keyframe() {
    let reg = Registry::new();
    let publ = reg.publish("live", cfg(50)).unwrap();
    publ.set_tracks(vec![video()]).unwrap();
    let mut sub = reg.subscribe("live", StartAt::LiveEdge).unwrap();
    assert_eq!(sub.try_recv(), Some(Event::TracksChanged));
    // Mid-GOP frames from a source that started between keyframes.
    for n in 25..30 {
        publ.push(vframe(n, 10)).unwrap();
    }
    assert_eq!(sub.try_recv(), None);
    publ.push(vframe(30, 10)).unwrap();
    let f = next_frame(&mut sub);
    assert!(f.keyframe);
    assert_eq!(f.dts, 30 * 3000);
}

#[test]
fn viewer_count_follows_subscribers() {
    let reg = Registry::new();
    let publ = reg.publish("live", cfg(50)).unwrap();
    let a = reg.subscribe("live", StartAt::LiveEdge).unwrap();
    let b = reg.subscribe("live", StartAt::LiveEdge).unwrap();
    assert_eq!(publ.stream().stats().viewers, 2);
    drop((a, b));
    assert_eq!(publ.stream().stats().viewers, 0);
}

#[tokio::test]
async fn recv_wakes_on_push_and_on_end() {
    let reg = Registry::new();
    let publ = reg.publish("live", cfg(50)).unwrap();
    publ.set_tracks(vec![video()]).unwrap();
    let mut sub = reg.subscribe("live", StartAt::LiveEdge).unwrap();
    assert_eq!(sub.recv().await, Event::TracksChanged);

    let reader = tokio::spawn(async move {
        let mut got = Vec::new();
        loop {
            match sub.recv().await {
                Event::Frame(f) => got.push(f.dts),
                Event::End => return got,
                other => panic!("unexpected {other:?}"),
            }
        }
    });
    for n in 0..60 {
        publ.push(vframe(n, 10)).unwrap();
        tokio::task::yield_now().await;
    }
    drop(publ);
    let got = tokio::time::timeout(Duration::from_secs(5), reader).await.unwrap().unwrap();
    assert_eq!(got, (0..60).map(|n| n * 3000).collect::<Vec<_>>());
}

#[test]
fn publishes_are_announced() {
    let reg = Registry::new();
    let mut rx = reg.subscribe_publishes();
    let _p = reg.publish("live", cfg(1)).unwrap();
    assert_eq!(rx.try_recv().unwrap().name(), "live");
}

#[test]
fn api_names() {
    assert_eq!(Codec::H264.as_str(), "h264");
    assert_eq!(Codec::Aac.kind().as_str(), "audio");
}
