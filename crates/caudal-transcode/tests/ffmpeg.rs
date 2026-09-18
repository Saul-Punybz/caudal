//! The ffmpeg engine end to end: a real ffmpeg transcoding a live source
//! published into a registry. Run one at a time (`--test-threads=1`).

mod support;

use std::sync::Arc;
use std::time::Duration;

use caudal_core::{BufferConfig, Codec, Event, Frame, Registry, StartAt, TrackId, TrackInfo};
use caudal_transcode::{Engine, Ladder, Rendition, TranscodeConfig};
use support::*;

fn config(streams: &[&str], renditions: &[(&str, u32)]) -> TranscodeConfig {
    TranscodeConfig {
        ladders: vec![Ladder {
            streams: streams.iter().map(|s| s.to_string()).collect(),
            renditions: renditions
                .iter()
                .map(|&(label, height)| Rendition { label: label.into(), height, video_kbps: 300, audio_kbps: 64 })
                .collect(),
        }],
        engine: Engine::Ffmpeg,
        ffmpeg: "ffmpeg".into(),
        buffer: BufferConfig::default(),
    }
}

struct Collected {
    tracks: Vec<TrackInfo>,
    frames: Vec<Frame>,
    ended: bool,
}

/// Reads rendition `name` from its oldest keyframe until it ends or
/// `timeout` passes.
async fn collect(registry: &Arc<Registry>, name: &str, timeout: Duration) -> Collected {
    let mut sub = registry.subscribe(name, StartAt::Oldest).expect("rendition stream");
    let mut c = Collected { tracks: Vec::new(), frames: Vec::new(), ended: false };
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match tokio::time::timeout_at(deadline, sub.recv()).await {
            Err(_) => return c,
            Ok(Event::End) => {
                c.ended = true;
                return c;
            }
            Ok(Event::TracksChanged) => c.tracks = sub.tracks(),
            Ok(Event::Lagged { .. }) => panic!("rendition reader lagged"),
            Ok(Event::Cue(_)) => {}
            Ok(Event::Frame(f)) => c.frames.push((*f).clone()),
        }
    }
}

fn micros(tracks: &[TrackInfo], f: &Frame) -> i64 {
    tracks.iter().find(|t| t.id == f.track).unwrap().to_micros(f.dts)
}

fn assert_monotonic(c: &Collected) {
    for id in [TrackId(0), TrackId(1)] {
        let dts: Vec<i64> = c.frames.iter().filter(|f| f.track == id).map(|f| f.dts).collect();
        assert!(dts.windows(2).all(|w| w[1] > w[0]), "dts not monotonic on {id:?}");
    }
}

async fn wait_no_ffmpeg(name: &str) {
    let gone = wait_for(Duration::from_secs(5), || ffmpeg_pids(name).is_empty().then_some(())).await;
    assert!(gone.is_some(), "ffmpeg for {name} still running: {:?}", ffmpeg_pids(name));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rendition_follows_the_source() {
    init_logs();
    let dir = scratch("main");
    let clip = Arc::new(testsrc_clip(&dir, 8));
    let registry = Registry::new();
    caudal_transcode::start(registry.clone(), config(&["tc_main"], &[("240p", 240)])).unwrap();
    // An arbitrary source clock, to prove renditions keep it.
    let offset_us = 123_456_789;
    let source = publish(&registry, "tc_main", clip.clone(), offset_us);

    let found = wait_for(Duration::from_secs(10), || registry.get("tc_main+240p")).await;
    assert!(found.is_some(), "tc_main+240p never appeared");
    let c = collect(&registry, "tc_main+240p", Duration::from_secs(15)).await;
    source.await.unwrap();
    assert!(c.ended, "rendition did not end with the source");

    let video = c.tracks.iter().find(|t| t.codec == Codec::H264).expect("h264 track");
    assert_eq!(video.video.unwrap().height, 240);
    assert_eq!(video.video.unwrap().width, 426);
    let audio = c.tracks.iter().find(|t| t.codec == Codec::Aac).expect("aac track");
    assert!(c.frames.len() > 100, "only {} frames", c.frames.len());
    assert_monotonic(&c);

    // Keyframes every ~2 s.
    let keys: Vec<i64> =
        c.frames.iter().filter(|f| f.track == video.id && f.keyframe).map(|f| micros(&c.tracks, f)).collect();
    assert!(keys.len() >= 3, "keyframes: {keys:?}");
    for w in keys.windows(2) {
        let gap = w[1] - w[0];
        assert!((1_900_000..=2_100_000).contains(&gap), "keyframe gap {gap} us: {keys:?}");
    }

    // Same clock as the source: every rendition video frame is a source frame.
    let src_video: Vec<i64> =
        clip.frames.iter().filter(|f| f.track == TrackId(0)).map(|f| clip.micros(f) + offset_us).collect();
    for f in c.frames.iter().filter(|f| f.track == video.id) {
        let m = micros(&c.tracks, f);
        let near = src_video.iter().map(|s| (s - m).abs()).min().unwrap();
        assert!(near <= 2_000, "rendition frame at {m} us is {near} us from any source frame");
    }
    let src_audio_first =
        clip.frames.iter().find(|f| f.track == TrackId(1)).map(|f| clip.micros(f) + offset_us).unwrap();
    let rend_audio: Vec<i64> = c.frames.iter().filter(|f| f.track == audio.id).map(|f| micros(&c.tracks, f)).collect();
    assert!(!rend_audio.is_empty(), "no audio frames");
    let src_video_last = *src_video.last().unwrap();
    for m in [rend_audio[0], *rend_audio.last().unwrap()] {
        assert!(m > src_audio_first - 500_000 && m < src_video_last + 500_000, "audio at {m} us off the source clock");
    }
    let first_video = micros(&c.tracks, c.frames.iter().find(|f| f.track == video.id).unwrap());
    assert!((rend_audio[0] - first_video).abs() < 500_000, "audio {} vs video {first_video}", rend_audio[0]);

    wait_no_ffmpeg("tc_main").await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ffmpeg_crash_is_restarted() {
    init_logs();
    let dir = scratch("crash");
    let clip = Arc::new(testsrc_clip(&dir, 10));
    let registry = Registry::new();
    caudal_transcode::start(registry.clone(), config(&["tc_*"], &[("180p", 180)])).unwrap();
    let source = publish(&registry, "tc_crash", clip.clone(), 0);

    assert!(wait_for(Duration::from_secs(10), || registry.get("tc_crash+180p")).await.is_some());
    let rendition = registry.get("tc_crash+180p").unwrap();
    let mut sub = rendition.subscribe_internal(StartAt::LiveEdge);
    let first = ffmpeg_pids("tc_crash");
    assert_eq!(first.len(), 1, "expected one ffmpeg: {first:?}");
    let killed = std::process::Command::new("kill").args(["-KILL", &first[0].to_string()]).status().unwrap();
    assert!(killed.success());

    let second = wait_for(Duration::from_secs(8), || {
        let p = ffmpeg_pids("tc_crash");
        (p.len() == 1 && p[0] != first[0]).then(|| p[0])
    })
    .await;
    assert!(second.is_some(), "ffmpeg was not restarted");
    assert!(!rendition.is_ended(), "rendition ended on an ffmpeg crash");

    // Frames keep flowing on the same rendition stream, dts still rising.
    let mut last = i64::MIN;
    let mut after_restart = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    while after_restart < 30 {
        match tokio::time::timeout_at(deadline, sub.recv()).await.expect("no frames after restart") {
            Event::Frame(f) if f.track == TrackId(0) => {
                assert!(f.dts > last, "dts went back after restart");
                last = f.dts;
                after_restart += 1;
            }
            Event::End => panic!("rendition ended"),
            _ => {}
        }
    }
    source.abort();
    let _ = source.await;
    wait_no_ffmpeg("tc_crash").await;
    assert!(wait_for(Duration::from_secs(5), || rendition.is_ended().then_some(())).await.is_some());
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn renditions_and_larger_rungs_are_never_transcoded() {
    init_logs();
    let dir = scratch("plus");
    let clip = Arc::new(testsrc_clip(&dir, 3));
    let registry = Registry::new();
    // `*` matches everything except names with `+`; 360p and 480p are not
    // smaller than the 360p source.
    caudal_transcode::start(registry.clone(), config(&["*"], &[("360p", 360), ("480p", 480)])).unwrap();
    let a = publish(&registry, "tc_src+x", clip.clone(), 0);
    let b = publish(&registry, "tc_big", clip.clone(), 0);
    tokio::time::sleep(Duration::from_secs(2)).await;
    let names: Vec<String> = registry.list().iter().map(|s| s.name().to_owned()).collect();
    assert_eq!(names, ["tc_big", "tc_src+x"]);
    assert!(ffmpeg_pids("tc_src+x").is_empty() && ffmpeg_pids("tc_big").is_empty());
    let _ = (a.await, b.await);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opus_source_gets_aac_renditions() {
    let clip = Arc::new(opus_clip(2));
    assert!(clip.tracks.iter().any(|t| t.codec == Codec::Opus));
    let registry = Registry::new();
    caudal_transcode::start(registry.clone(), config(&["tc_opus"], &[("96p", 96)])).unwrap();
    let source = publish(&registry, "tc_opus", clip.clone(), 0);
    assert!(wait_for(Duration::from_secs(10), || registry.get("tc_opus+96p")).await.is_some());
    let c = collect(&registry, "tc_opus+96p", Duration::from_secs(12)).await;
    source.await.unwrap();
    assert!(c.ended);
    let video = c.tracks.iter().find(|t| t.codec == Codec::H264).expect("h264");
    assert_eq!(video.video.unwrap().height, 96);
    let audio = c.tracks.iter().find(|t| t.codec == Codec::Aac).expect("opus became aac");
    assert_eq!(audio.timescale, 48_000);
    assert!(c.frames.iter().filter(|f| f.track == audio.id).count() > 100);
    assert!(c.frames.iter().filter(|f| f.track == video.id).count() > 100);
    assert_monotonic(&c);
    wait_no_ffmpeg("tc_opus").await;
}
