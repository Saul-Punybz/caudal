//! The raw-frame feed end to end with a real ffmpeg: a synthetic UYVY
//! pattern + planar float tone, stamped like an OMT source and mapped with
//! `TimeMap`, comes out as an H.264 + AAC stream in the registry on the
//! same clock. Skipped when ffmpeg is not on PATH.

use std::time::{Duration, Instant};

use bytes::Bytes;
use caudal_core::{BufferConfig, Codec, Event, Frame, Registry, StartAt, TrackInfo};
use caudal_omt::time::OMT_HZ;
use caudal_omt::{AudioFrame, Feed, FeedConfig, PixelLayout, SampleLayout, TimeMap, VideoFormat, VideoFrame};

const W: u32 = 320;
const H: u32 = 180;
const FPS: i64 = 30;
const RATE: u32 = 48_000;
const FRAMES: i64 = 90;

fn have_ffmpeg() -> bool {
    std::process::Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success())
}

fn init_logs() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}

/// A UYVY picture: grey ramp with a white bar that moves one step a frame.
fn pattern(i: i64) -> Bytes {
    let (w, h) = (W as usize, H as usize);
    let mut data = vec![0u8; w * h * 2];
    let bar = (i as usize * 4) % w;
    for y in 0..h {
        for x in (0..w).step_by(2) {
            let luma = if (bar..bar + 8).contains(&x) { 235 } else { 16 + (x * 200 / w) as u8 };
            let o = (y * w + x) * 2;
            data[o..o + 4].copy_from_slice(&[128, luma, 128, luma]);
        }
    }
    Bytes::from(data)
}

/// `n` samples of a 440 Hz tone from sample `start`, planar stereo.
fn tone(start: i64, n: usize) -> Bytes {
    let mut data = Vec::with_capacity(n * 8);
    for _ch in 0..2 {
        for k in 0..n {
            let t = (start + k as i64) as f32 / RATE as f32;
            data.extend_from_slice(&(0.3 * (2.0 * std::f32::consts::PI * 440.0 * t).sin()).to_le_bytes());
        }
    }
    Bytes::from(data)
}

async fn wait_for<T>(timeout: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = f() {
            return Some(v);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn ffmpeg_running(stream: &str) -> bool {
    std::process::Command::new("pgrep")
        .args(["-f", &format!("service_name={stream} pipe:1")])
        .output()
        .is_ok_and(|o| !o.stdout.is_empty())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pattern_and_tone_come_out_as_h264_and_aac_on_the_same_clock() {
    if !have_ffmpeg() {
        eprintln!("SKIP: ffmpeg not on PATH");
        return;
    }
    init_logs();
    let name = "omt_feed_test";
    let registry = Registry::new();
    let feed = Feed::start(
        registry.clone(),
        FeedConfig {
            ffmpeg: "ffmpeg".into(),
            stream: name.into(),
            buffer: BufferConfig::default(),
            video_kbps: 400,
            audio_kbps: 64,
            expect_audio: true,
        },
    )
    .unwrap();

    // An OMT-like source: 100 ns stamps from an arbitrary clock, audio in
    // 1600-sample chunks (one per video frame) starting at the same instant.
    let t0: i64 = 1_234_567_890_123;
    let frame_100ns = OMT_HZ / FPS;
    let chunk = (RATE as i64 / FPS) as usize;
    let format = VideoFormat { width: W, height: H, layout: PixelLayout::Uyvy, fps_num: FPS as u32, fps_den: 1 };
    let mut map = TimeMap::new();
    let mut video_in = Vec::new();
    let mut audio_in = Vec::new();
    for i in 0..FRAMES {
        let ts = t0 + i * frame_100ns;
        let a = map.audio(ts, chunk as u32, RATE).expect("audio pts");
        audio_in.push(a);
        feed.push_audio(AudioFrame {
            sample_rate: RATE,
            channels: 2,
            layout: SampleLayout::Planar,
            pts: a,
            data: tone(a, chunk),
        })
        .await
        .unwrap();
        let v = map.video(ts, frame_100ns);
        video_in.push(v);
        feed.push_video(VideoFrame { format, pts: v, data: pattern(i) }).await.unwrap();
    }
    assert_eq!(video_in[1], 3000);
    assert_eq!(audio_in[0], 0);

    let stream = wait_for(Duration::from_secs(20), || registry.get(name).filter(|s| s.tracks().len() == 2))
        .await
        .expect("stream with two tracks");
    let mut sub = registry.subscribe(name, StartAt::Oldest).unwrap();
    let tracks: Vec<TrackInfo> = stream.tracks();
    let finish = tokio::spawn(feed.finish());

    let mut frames: Vec<Frame> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        match tokio::time::timeout_at(deadline, sub.recv()).await.expect("stream never ended") {
            Event::End => break,
            Event::Frame(f) => frames.push((*f).clone()),
            Event::Lagged { .. } => panic!("reader lagged"),
            Event::TracksChanged | Event::Cue(_) => {}
        }
    }
    finish.await.unwrap();

    let video = tracks.iter().find(|t| t.codec == Codec::H264).expect("H.264 track");
    let audio = tracks.iter().find(|t| t.codec == Codec::Aac).expect("AAC track");
    let vp = video.video.unwrap();
    assert_eq!((vp.width, vp.height), (W, H));
    assert_eq!(audio.audio.unwrap().sample_rate, RATE);
    assert_eq!(video.timescale, 90_000);

    let vpts: Vec<i64> = frames.iter().filter(|f| f.track == video.id).map(|f| f.pts).collect();
    let adts: Vec<i64> = frames.iter().filter(|f| f.track == audio.id).map(|f| f.dts).collect();
    assert!(vpts.len() as i64 >= FRAMES - 5, "only {} of {FRAMES} video frames", vpts.len());
    assert!(adts.len() >= 100, "only {} AAC frames", adts.len());
    assert!(vpts.windows(2).all(|w| w[1] > w[0]), "video pts not increasing: {vpts:?}");
    assert!(adts.windows(2).all(|w| w[1] > w[0]), "audio dts not increasing");

    // Same clock as the input: every picture lands on an input frame time.
    for p in &vpts {
        assert!(video_in.iter().any(|v| (v - p).abs() <= 1), "video pts {p} is not an input frame time");
    }
    // A/V offset: first picture vs first AAC frame, both started at t0.
    let v0 = video.to_micros(vpts[0]) - video.to_micros(video_in[0]);
    let a0 = audio.to_micros(adts[0]) - audio.to_micros(audio_in[0]);
    let frame_us = 1_000_000 / FPS;
    eprintln!(
        "video {} frames, AAC {} frames; first picture {v0} us, first AAC {a0} us after the input's; A/V offset {} us",
        vpts.len(),
        adts.len(),
        v0 - a0
    );
    assert!((v0 - a0).abs() < frame_us, "A/V offset {} us (video {v0} us, audio {a0} us)", v0 - a0);
    // Last audio covers the tone to within a frame of the last picture.
    let v_end = video.to_micros(*vpts.last().unwrap());
    let a_end = audio.to_micros(*adts.last().unwrap());
    assert!((v_end - a_end).abs() < 3 * frame_us, "ends: video {v_end} us, audio {a_end} us");

    assert!(
        wait_for(Duration::from_secs(5), || (!ffmpeg_running(name)).then_some(())).await.is_some(),
        "ffmpeg still running after finish"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_feed_kills_ffmpeg_and_ends_the_stream() {
    if !have_ffmpeg() {
        eprintln!("SKIP: ffmpeg not on PATH");
        return;
    }
    init_logs();
    let name = "omt_feed_drop";
    let registry = Registry::new();
    let feed = Feed::start(
        registry.clone(),
        FeedConfig {
            ffmpeg: "ffmpeg".into(),
            stream: name.into(),
            buffer: BufferConfig::default(),
            video_kbps: 200,
            audio_kbps: 64,
            expect_audio: false,
        },
    )
    .unwrap();
    let format = VideoFormat { width: W, height: H, layout: PixelLayout::Uyvy, fps_num: 30, fps_den: 1 };
    for i in 0..30 {
        feed.push_video(VideoFrame { format, pts: i * 3000, data: pattern(i) }).await.unwrap();
    }
    let stream = wait_for(Duration::from_secs(20), || registry.get(name)).await.expect("stream published");
    assert!(ffmpeg_running(name));
    drop(feed);
    assert!(
        wait_for(Duration::from_secs(5), || (!ffmpeg_running(name)).then_some(())).await.is_some(),
        "ffmpeg still running after drop"
    );
    assert!(wait_for(Duration::from_secs(5), || stream.is_ended().then_some(())).await.is_some(), "stream not ended");
}
