//! The in-process `RustyH264` engine on the small H.264 (B-frames) + AAC
//! fixture. Debug builds of the codec are slow, hence the tiny source.

mod support;

use std::sync::Arc;
use std::time::Duration;

use caudal_core::{BufferConfig, Codec, Event, StartAt, TrackId};
use caudal_transcode::{Engine, Ladder, Rendition, TranscodeConfig};
use support::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rusty_h264_rendition() {
    init_logs();
    let bytes = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../caudal-hls/tests/fixtures/av.mp4")).unwrap();
    let d = mp4demux::demux(&bytes);
    let frames = (0..2).flat_map(|n| d.looped(n).collect::<Vec<_>>()).collect();
    let clip = Arc::new(Clip { tracks: d.tracks.clone(), frames });
    let registry = caudal_core::Registry::new();
    let cfg = TranscodeConfig {
        ladders: vec![Ladder {
            streams: vec!["tr_src".into()],
            renditions: vec![Rendition { label: "96p".into(), height: 96, video_kbps: 150, audio_kbps: 64 }],
        }],
        engine: Engine::RustyH264,
        ffmpeg: "ffmpeg".into(),
        buffer: BufferConfig::default(),
    };
    caudal_transcode::start(registry.clone(), cfg).unwrap();
    let source = publish(&registry, "tr_src", clip.clone(), 0);
    assert!(wait_for(Duration::from_secs(10), || registry.get("tr_src+96p")).await.is_some());
    let mut sub = registry.subscribe("tr_src+96p", StartAt::Oldest).unwrap();
    let mut tracks = Vec::new();
    let mut got = Vec::new();
    let ended = loop {
        match tokio::time::timeout(Duration::from_secs(15), sub.recv()).await {
            Err(_) => break false,
            Ok(Event::End) => break true,
            Ok(Event::TracksChanged) => tracks = sub.tracks(),
            Ok(Event::Frame(f)) => got.push((*f).clone()),
            Ok(Event::Lagged { .. }) => panic!("lagged"),
        }
    };
    source.await.unwrap();
    assert!(ended, "rendition did not end with the source");
    let video = tracks.iter().find(|t| t.codec == Codec::H264).expect("h264");
    assert_eq!((video.video.unwrap().width, video.video.unwrap().height), (170, 96));
    let audio = tracks.iter().find(|t| t.codec == Codec::Aac).expect("aac passthrough");
    let src_audio = clip.tracks.iter().find(|t| t.codec == Codec::Aac).unwrap();
    assert_eq!(audio.init, src_audio.init);

    let v: Vec<_> = got.iter().filter(|f| f.track == video.id).collect();
    let src_v: Vec<i64> =
        clip.frames.iter().filter(|f| f.track == TrackId(0)).map(|f| f.pts * 1_000_000 / 90_000).collect();
    eprintln!("rusty_h264 (debug build): {} of {} video frames transcoded", v.len(), src_v.len());
    assert!(v.len() * 2 > src_v.len(), "only {} of {} frames", v.len(), src_v.len());
    assert!(v.windows(2).all(|w| w[1].dts > w[0].dts), "video dts not monotonic");
    // Output pts == the source picture's pts.
    for f in &v {
        let m = video.to_micros(f.pts);
        assert!(src_v.iter().any(|s| (s - m).abs() <= 1), "frame at {m} us is not a source frame");
    }
    let keys: Vec<i64> = v.iter().filter(|f| f.keyframe).map(|f| video.to_micros(f.pts)).collect();
    assert!(keys.len() >= 2, "{keys:?}");
    for w in keys.windows(2) {
        assert!((1_900_000..=2_200_000).contains(&(w[1] - w[0])), "keyframes {keys:?}");
    }
    assert!(got.iter().filter(|f| f.track == audio.id).count() > 100);
}
