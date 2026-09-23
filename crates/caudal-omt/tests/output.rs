//! OMT output end to end over loopback (no mDNS): a stream in the registry
//! → `start_outputs` → our OMT `Receiver` + `MediaDecoder` on
//! `127.0.0.1:port`. Pictures are checked against what went in (PSNR),
//! audio for its rate, channels and tone. The ffmpeg paths (High profile
//! H.264, AAC through ffmpeg, Opus with B-frame video) use the MP4
//! fixtures and are skipped when ffmpeg is not on PATH.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use bytes::Bytes;
use caudal_core::{AudioParams, BufferConfig, Codec, Frame, Registry, TrackId, TrackInfo, VideoParams};
use caudal_omt::{OutputConfig, OutputHandle, OutputOptions, OutputStats, start_outputs};
use mp4_atom::{Atom, Avcc};
use open_media_transport::media::{Media, MediaDecoder, PreferredVideoFormat, VideoFormat};
use open_media_transport::receiver::{Event, Receiver, ReceiverConfig};

#[path = "../../caudal-hls/src/mp4demux.rs"]
#[allow(dead_code)]
mod mp4demux;

const W: usize = 320;
const H: usize = 180;
const FPS: i64 = 30;
/// Frames in the synthetic clip (it loops); keyframe every `GOP`.
const CLIP: usize = 64;
const GOP: u32 = 16;
const RATE: u32 = 48_000;
const TONE_HZ: f32 = 440.0;

fn init_logs() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}

fn have_ffmpeg() -> bool {
    std::process::Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success())
}

/// Luma of synthetic frame `i`: a horizontal ramp with a bright bar that
/// moves 4 px a frame.
fn luma(i: usize) -> Vec<u8> {
    let bar = (i * 4) % W;
    let mut y = vec![0u8; W * H];
    for row in 0..H {
        for x in 0..W {
            y[row * W + x] =
                if (bar..bar + 16).contains(&x) { 235 } else { 16 + (x * 150 / W) as u8 + (row * 40 / H) as u8 };
        }
    }
    y
}

/// NAL units of an Annex B stream, without start codes.
fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push((i, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::new();
    for (k, &(_, begin)) in starts.iter().enumerate() {
        let mut end = starts.get(k + 1).map_or(data.len(), |&(code, _)| code);
        while end > begin && data[end - 1] == 0 {
            end -= 1;
        }
        if end > begin {
            nals.push(&data[begin..end]);
        }
    }
    nals
}

struct Clip {
    tracks: Vec<TrackInfo>,
    /// One loop, in push order.
    frames: Vec<Frame>,
    /// Length of one loop per track, in that track's timescale.
    len: Vec<(TrackId, i64)>,
}

impl Clip {
    fn micros(&self, f: &Frame) -> i64 {
        self.tracks.iter().find(|t| t.id == f.track).unwrap().to_micros(f.dts)
    }
}

/// The synthetic clip: `CLIP` frames of Constrained Baseline H.264
/// (`rusty_h264`, as Caudal's RustyH264 renditions) + stereo AAC-LC tone.
fn synthetic_clip() -> Clip {
    let mut c = rusty_h264::EncoderConfig::new(W, H);
    c.bitrate = 3_000_000;
    c.framerate = FPS as f32;
    c.gop_size = GOP;
    c.scenecut = 0;
    c.lookahead = 0;
    c.bframes = 0;
    let mut enc = rusty_h264::Encoder::new(c).unwrap();
    let (mut sps, mut pps, mut video) = (None, None, Vec::new());
    for i in 0..CLIP {
        let mut pic = rusty_h264::YuvFrame::black(W, H);
        pic.y = luma(i);
        pic.u.fill(128);
        pic.v.fill(128);
        let au = enc.try_encode(&pic).unwrap();
        let (mut data, mut idr) = (Vec::new(), false);
        for nal in split_annexb(&au) {
            match nal[0] & 0x1F {
                7 => sps = Some(nal.to_vec()),
                8 => pps = Some(nal.to_vec()),
                9 => {}
                t => {
                    idr |= t == 5;
                    data.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                    data.extend_from_slice(nal);
                }
            }
        }
        let ts = i as i64 * 90_000 / FPS;
        video.push(Frame { track: TrackId(0), dts: ts, pts: ts, keyframe: idr, data: Bytes::from(data) });
    }
    assert!(video[0].keyframe);
    let mut avcc = Vec::new();
    Avcc::new(&sps.unwrap(), &pps.unwrap()).unwrap().encode_body(&mut avcc).unwrap();

    // One loop of audio: 64 frames at 30 fps = 102 400 samples = exactly
    // 100 AAC frames, so the looped audio has no gaps.
    let loop_samples = CLIP * RATE as usize / FPS as usize;
    let mut aenc =
        rusty_aac::AacEncoder::new(rusty_aac::AacEncoderConfig { bitrate_bps: 128_000, ..Default::default() });
    let pcm: Vec<f32> = (0..loop_samples)
        .flat_map(|k| {
            let s = 0.3 * (2.0 * std::f32::consts::PI * TONE_HZ * k as f32 / RATE as f32).sin();
            [s, s]
        })
        .collect();
    aenc.push_pcm(&pcm, 2, RATE).unwrap();
    aenc.finish();
    let mut audio = Vec::new();
    while let Ok(p) = aenc.next_packet() {
        if p.pts >= 0 && (p.pts as usize) + 1024 <= loop_samples {
            audio.push(Frame { track: TrackId(1), dts: p.pts, pts: p.pts, keyframe: true, data: Bytes::from(p.data) });
        }
    }
    assert!(audio.len() > 80, "{} AAC frames", audio.len());

    let tracks = vec![
        TrackInfo {
            id: TrackId(0),
            codec: Codec::H264,
            timescale: 90_000,
            init: Bytes::from(avcc),
            lang: None,
            video: Some(VideoParams { width: W as u32, height: H as u32, fps: Some(FPS as f64) }),
            audio: None,
        },
        TrackInfo {
            id: TrackId(1),
            codec: Codec::Aac,
            timescale: RATE,
            init: rusty_aac::audio_specific_config_bytes(RATE, 2).into(),
            lang: None,
            video: None,
            audio: Some(AudioParams { sample_rate: RATE, channels: 2 }),
        },
    ];
    let mut frames: Vec<Frame> = video.into_iter().chain(audio).collect();
    let micros = |f: &Frame| tracks[f.track.0 as usize].to_micros(f.dts);
    frames.sort_by_key(|f| (micros(f), f.track.0));
    let len = vec![(TrackId(0), CLIP as i64 * 90_000 / FPS), (TrackId(1), loop_samples as i64)];
    Clip { tracks, frames, len }
}

/// An MP4 fixture as a clip (one loop = the video length).
fn fixture_clip(file: &str) -> Clip {
    let bytes = std::fs::read(format!("{}/../caudal-hls/tests/fixtures/{file}", env!("CARGO_MANIFEST_DIR"))).unwrap();
    let d = mp4demux::demux(&bytes);
    let frames: Vec<Frame> = d.looped(0).collect();
    let len_us = d.video_len * 1_000_000 / 90_000;
    let len = d.tracks.iter().map(|t| (t.id, len_us * i64::from(t.timescale) / 1_000_000)).collect();
    Clip { tracks: d.tracks.clone(), frames, len }
}

/// Publishes `clip` as `name`, looping, paced in real time, until aborted.
fn publish(registry: &Arc<Registry>, name: &str, clip: Arc<Clip>) -> tokio::task::JoinHandle<()> {
    let publisher = registry.publish(name, BufferConfig::default()).unwrap();
    publisher.set_tracks(clip.tracks.clone()).unwrap();
    tokio::spawn(async move {
        let start = tokio::time::Instant::now();
        let loop_us = clip.len.iter().find(|(id, _)| *id == clip.frames[0].track).map_or(0, |&(id, l)| {
            clip.tracks.iter().find(|t| t.id == id).unwrap().to_micros(l)
        });
        for n in 0.. {
            for f in &clip.frames {
                let at = n * loop_us + clip.micros(f);
                tokio::time::sleep_until(start + Duration::from_micros(at.max(0) as u64)).await;
                let shift = n * clip.len.iter().find(|(id, _)| *id == f.track).unwrap().1;
                let _ = publisher.push(Frame { dts: f.dts + shift, pts: f.pts + shift, ..f.clone() });
            }
        }
    })
}

fn output(stream: &str, name: &str) -> OutputConfig {
    OutputConfig { announce: false, ..OutputConfig::new(stream, name) }
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
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn port_of(handle: &OutputHandle, name: &str) -> (u16, Arc<OutputStats>) {
    wait_for(Duration::from_secs(5), || {
        let o = handle.outputs().into_iter().find(|o| o.name == name)?;
        Some((o.stats.port()?, o.stats))
    })
    .await
    .expect("sender did not start")
}

/// Pictures (as luma planes with their timestamps) and audio received.
#[derive(Default)]
struct Received {
    video: Vec<(i64, usize, usize, Vec<u8>)>,
    /// (timestamp, rate, channels, samples per channel, channel 0).
    audio: Vec<(i64, i32, usize, usize, Vec<f32>)>,
}

/// Receives on `127.0.0.1:port` until `until` holds or `timeout` passes
/// (blocking; run it on a blocking thread).
fn receive(port: u16, timeout: Duration, until: impl Fn(&Received) -> bool) -> Received {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let rx = Receiver::connect(addr, ReceiverConfig { reconnect: false, ..ReceiverConfig::default() }).unwrap();
    let mut dec = MediaDecoder::new(PreferredVideoFormat::Uyvy);
    let mut got = Received::default();
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline && !until(&got) {
        let Some(Event::Frame(_, f)) = rx.recv_timeout(Duration::from_millis(100)) else { continue };
        match dec.decode(&f) {
            Ok(Some(Media::Video(v))) => {
                assert_eq!(v.format, VideoFormat::Uyvy);
                let y: Vec<u8> = v.data.chunks_exact(2).map(|p| p[1]).take(v.width * v.height).collect();
                got.video.push((v.timestamp, v.width, v.height, y));
            }
            Ok(Some(Media::Audio(a))) => {
                got.audio.push((a.timestamp, a.sample_rate, a.channels, a.samples_per_channel, a.channel(0).to_vec()))
            }
            _ => {}
        }
    }
    drop(rx);
    got
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let mse = a.iter().zip(b).map(|(&x, &y)| (f64::from(x) - f64::from(y)).powi(2)).sum::<f64>() / a.len() as f64;
    if mse == 0.0 { 99.0 } else { 10.0 * (255.0f64 * 255.0 / mse).log10() }
}

/// Zero crossings per second of `samples` at `rate`, halved: the frequency
/// of a pure tone.
fn tone_hz(samples: &[f32], rate: f64) -> f64 {
    let crossings = samples.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count();
    crossings as f64 / 2.0 / (samples.len() as f64 / rate)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn baseline_h264_and_aac_reach_an_omt_receiver() {
    init_logs();
    let registry = Registry::new();
    let clip = Arc::new(synthetic_clip());
    // The output starts first and waits for the stream.
    let handle = start_outputs(registry.clone(), OutputOptions::default(), vec![output("synth", "Synth")]);
    let (port, stats) = port_of(&handle, "Synth").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!stats.attached.load(Ordering::Relaxed));
    let feeder = publish(&registry, "synth", clip);

    // No receivers: frames arrive, nothing is decoded or encoded.
    wait_for(Duration::from_secs(3), || (stats.video_skipped.load(Ordering::Relaxed) >= 20).then_some(())).await.unwrap();
    assert_eq!(stats.video_sent.load(Ordering::Relaxed), 0, "encoded with no receivers");
    assert_eq!(stats.audio_sent.load(Ordering::Relaxed), 0, "decoded audio with no receivers");
    assert_eq!(registry.get("synth").unwrap().stats().viewers, 0);

    let got = tokio::task::spawn_blocking(move || {
        receive(port, Duration::from_secs(8), |r| {
            r.video.len() >= 45 && r.audio.iter().map(|a| a.3).sum::<usize>() >= RATE as usize
        })
    });
    // While connected, the receiver counts as a viewer of the stream.
    let viewers = wait_for(Duration::from_secs(3), || {
        Some(registry.get("synth")?.stats().viewers).filter(|&v| v >= 1)
    })
    .await;
    assert_eq!(viewers, Some(1), "OMT receiver not counted as a viewer");
    let got = got.await.unwrap();

    assert!(got.video.len() >= 45, "only {} pictures", got.video.len());
    let mut worst = f64::MAX;
    for (ts, w, h, y) in &got.video {
        assert_eq!((*w, *h), (W, H));
        // Timestamps: the stream's pts in µs * 10, so the frame index is exact.
        let us = ts / 10;
        let idx = (us * FPS + 500_000) / 1_000_000;
        assert_eq!(us, idx * 1_000_000 / FPS, "timestamp {ts} is not on the 30 fps grid");
        worst = worst.min(psnr(y, &luma(idx as usize % CLIP)));
    }
    println!("synthetic: {} pictures, worst luma PSNR {worst:.1} dB", got.video.len());
    assert!(worst > 30.0, "worst luma PSNR {worst:.1} dB");
    assert!(got.video.windows(2).all(|w| w[1].0 > w[0].0), "video timestamps not increasing");

    let audio = &got.audio;
    assert!(!audio.is_empty(), "no audio");
    assert!(audio.iter().all(|a| a.1 == RATE as i32 && a.2 == 2), "rate/channels {:?}", (audio[0].1, audio[0].2));
    // Sample counting: each chunk starts where the previous one ended.
    for w in audio.windows(2) {
        let expect = w[0].0 + (w[0].3 as i64 * 10_000_000) / i64::from(RATE);
        assert!((w[1].0 - expect).abs() <= 10, "audio gap: {} then {}", w[0].0, w[1].0);
    }
    same_clock("synthetic", &got);
    let pcm: Vec<f32> = audio.iter().skip(2).flat_map(|a| a.4.iter().copied()).collect();
    let hz = tone_hz(&pcm, f64::from(RATE));
    assert!((hz - f64::from(TONE_HZ)).abs() < 5.0, "tone at {hz:.1} Hz");

    // The receiver left: back to skipping.
    let skipped = stats.video_skipped.load(Ordering::Relaxed);
    let sent = stats.video_sent.load(Ordering::Relaxed);
    wait_for(Duration::from_secs(3), || (stats.video_skipped.load(Ordering::Relaxed) > skipped + 10).then_some(()))
        .await
        .expect("still encoding after the receiver left");
    assert!(stats.video_sent.load(Ordering::Relaxed) <= sent + 10);
    wait_for(Duration::from_secs(3), || (registry.get("synth")?.stats().viewers == 0).then_some(()))
        .await
        .expect("viewer count not released");

    let t = Instant::now();
    handle.shutdown().await;
    assert!(t.elapsed() < Duration::from_secs(3), "shutdown took {:?}", t.elapsed());
    assert!(handle.outputs().is_empty());
    feeder.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_keeps_unchanged_outputs_and_closes_removed_ones() {
    init_logs();
    let registry = Registry::new();
    let feeder = publish(&registry, "cam", Arc::new(synthetic_clip()));
    let handle = start_outputs(registry.clone(), OutputOptions::default(), vec![output("cam", "Cam")]);
    let (port, stats) = port_of(&handle, "Cam").await;
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let rx = Receiver::connect(addr, ReceiverConfig { reconnect: false, audio: false, ..ReceiverConfig::default() })
        .unwrap();
    wait_for(Duration::from_secs(3), || (stats.video_sent.load(Ordering::Relaxed) > 0).then_some(()))
        .await
        .expect("no video sent");

    // Same config: untouched (same stats, still connected).
    handle.reload(&registry, OutputOptions::default(), vec![output("cam", "Cam")]);
    let same = handle.outputs();
    assert_eq!(same.len(), 1);
    assert!(Arc::ptr_eq(&same[0].stats, &stats));

    // Removed: the sender closes the connection promptly.
    let t = Instant::now();
    handle.reload(&registry, OutputOptions::default(), vec![]);
    assert!(handle.outputs().is_empty());
    let closed = tokio::task::spawn_blocking(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(Event::Closed(..)) = rx.recv_timeout(Duration::from_millis(100)) {
                return true;
            }
        }
        false
    })
    .await
    .unwrap();
    assert!(closed, "receiver still connected after the output was removed");
    assert!(t.elapsed() < Duration::from_secs(4), "took {:?}", t.elapsed());

    // A changed config restarts on a new sender.
    handle.reload(&registry, OutputOptions::default(), vec![output("cam", "Cam 2")]);
    let (port2, _) = port_of(&handle, "Cam 2").await;
    assert!(port2 > 0);
    handle.shutdown().await;
    feeder.abort();
}

/// ffmpeg's own decode of the `av.mp4` fixture: luma planes in display order.
fn reference_pictures() -> Vec<Vec<u8>> {
    let path = format!("{}/../caudal-hls/tests/fixtures/av.mp4", env!("CARGO_MANIFEST_DIR"));
    let out = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i", &path, "-f", "rawvideo", "-pix_fmt", "yuv420p", "-"])
        .output()
        .unwrap();
    let (fw, fh) = (256usize, 144usize);
    out.stdout.chunks_exact(fw * fh * 3 / 2).map(|f| f[..fw * fh].to_vec()).collect()
}

/// Checks pictures of the looped `av.mp4` against ffmpeg's decode, matching
/// them by timestamp; returns the worst luma PSNR.
fn fixture_psnr(label: &str, clip: &Clip, refs: &[Vec<u8>], got: &Received) -> f64 {
    let vtrack = clip.tracks.iter().find(|t| t.codec == Codec::H264).unwrap();
    let first_us = clip.frames.iter().filter(|f| f.track == vtrack.id).map(|f| vtrack.to_micros(f.pts)).min().unwrap();
    let loop_us = vtrack.to_micros(clip.len.iter().find(|(id, _)| *id == vtrack.id).unwrap().1);
    let mut worst = f64::MAX;
    let mut log = Vec::new();
    for (ts, w, h, y) in &got.video {
        assert_eq!((*w, *h), (256, 144), "{label}");
        let rel = (ts / 10 - first_us).rem_euclid(loop_us);
        let idx = ((rel * 30 + 500_000) / 1_000_000) as usize % refs.len();
        let p = psnr(y, &refs[idx]);
        let best = (0..refs.len()).max_by(|&a, &b| psnr(y, &refs[a]).total_cmp(&psnr(y, &refs[b]))).unwrap();
        log.push(format!("ts={ts} idx={idx} psnr={p:.1} best={best}"));
        worst = worst.min(p);
    }
    println!("{label}: {} pictures, worst luma PSNR {worst:.1} dB", got.video.len());
    assert!(worst > 30.0, "{label}: worst luma PSNR {worst:.1} dB against ffmpeg's decode\n{}", log.join("\n"));
    // B-frames: pictures come out in display order with their own pts.
    assert!(got.video.windows(2).all(|w| w[1].0 > w[0].0), "{label}: video timestamps not increasing");
    worst
}

fn check_audio(label: &str, got: &Received) {
    assert!(!got.audio.is_empty(), "{label}: no audio");
    assert!(got.audio.iter().all(|a| a.1 == 48_000 && a.2 == 1), "{label}: {:?}", (got.audio[0].1, got.audio[0].2));
    assert!(got.audio.iter().map(|a| a.3).sum::<usize>() >= 48_000, "{label}: under 1 s of audio");
    same_clock(label, got);
}

/// Audio and video share the stream's clock: the first picture (which
/// waits for a keyframe) falls within the audio received, give or take
/// 200 ms.
fn same_clock(label: &str, got: &Received) {
    let Some(v0) = got.video.first().map(|v| v.0) else { return };
    let (a0, a1) = (got.audio[0].0, got.audio.last().unwrap().0);
    assert!((a0 - 2_000_000..=a1 + 2_000_000).contains(&v0), "{label}: video {v0} outside audio {a0}..{a1}");
}

fn enough(r: &Received) -> bool {
    r.video.len() >= 40 && r.audio.iter().map(|a| a.3).sum::<usize>() >= 48_000
}

/// High profile with B-frames (the MP4 fixtures) decoded in process, with
/// AAC-LC and Opus audio.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn high_profile_with_bframes_aac_and_opus() {
    init_logs();
    let registry = Registry::new();
    let aac = Arc::new(fixture_clip("av.mp4"));
    let opus = Arc::new(fixture_clip("av_opus.mp4"));
    let feeders = [publish(&registry, "fx-aac", aac.clone()), publish(&registry, "fx-opus", opus.clone())];
    let handle = start_outputs(
        registry.clone(),
        OutputOptions::default(),
        vec![output("fx-aac", "Fx AAC"), output("fx-opus", "Fx Opus")],
    );
    let (port_aac, stats_aac) = port_of(&handle, "Fx AAC").await;
    let (port_opus, _) = port_of(&handle, "Fx Opus").await;
    let (got_aac, got_opus) = tokio::join!(
        tokio::task::spawn_blocking(move || receive(port_aac, Duration::from_secs(10), enough)),
        tokio::task::spawn_blocking(move || receive(port_opus, Duration::from_secs(10), enough)),
    );
    let (got_aac, got_opus) = (got_aac.unwrap(), got_opus.unwrap());
    assert!(got_aac.video.len() >= 40 && got_opus.video.len() >= 40, "too few pictures");
    if have_ffmpeg() {
        let refs = reference_pictures();
        fixture_psnr("rusty_h264", &aac, &refs, &got_aac);
    }
    assert_eq!(stats_aac.errors.load(Ordering::Relaxed), 0, "errors on the AAC output");
    check_audio("aac", &got_aac);
    check_audio("opus", &got_opus);
    handle.shutdown().await;
    for f in feeders {
        f.abort();
    }
}

/// The same fixture with video and AAC decoded by ffmpeg.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn video_and_aac_through_ffmpeg() {
    init_logs();
    if !have_ffmpeg() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    let registry = Registry::new();
    let aac = Arc::new(fixture_clip("av.mp4"));
    let feeder = publish(&registry, "fx-ff", aac.clone());
    let options = OutputOptions { ffmpeg: Some("ffmpeg".into()), decode_with_ffmpeg: true };
    let handle = start_outputs(registry.clone(), options, vec![output("fx-ff", "Fx ffmpeg")]);
    let (port, stats) = port_of(&handle, "Fx ffmpeg").await;
    let got = tokio::task::spawn_blocking(move || receive(port, Duration::from_secs(10), enough)).await.unwrap();
    assert!(got.video.len() >= 40, "only {} pictures", got.video.len());
    fixture_psnr("ffmpeg", &aac, &reference_pictures(), &got);
    assert_eq!(stats.errors.load(Ordering::Relaxed), 0, "errors");
    check_audio("aac via ffmpeg", &got);
    handle.shutdown().await;
    feeder.abort();
}

/// libomtnet (the reference .NET implementation) receives our output:
/// `interop/libomtnet-harness recv omt://127.0.0.1:PORT 5` from the OMT
/// repo. Run by hand with `CAUDAL_OMT_HARNESS=/path/to/libomtnet-harness
/// cargo test -p caudal-omt --test output -- --ignored libomtnet`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the libomtnet harness (CAUDAL_OMT_HARNESS)"]
async fn libomtnet_receives_the_output() {
    init_logs();
    let Ok(harness) = std::env::var("CAUDAL_OMT_HARNESS") else {
        eprintln!("CAUDAL_OMT_HARNESS not set; skipping");
        return;
    };
    let registry = Registry::new();
    let feeder = publish(&registry, "synth", Arc::new(synthetic_clip()));
    let handle = start_outputs(registry.clone(), OutputOptions::default(), vec![output("synth", "Synth")]);
    let (port, stats) = port_of(&handle, "Synth").await;
    let out = tokio::process::Command::new(&harness)
        .args(["recv", &format!("omt://127.0.0.1:{port}"), "5"])
        .output()
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    println!("{text}");
    let done = text.lines().find(|l| l.starts_with("recv done")).expect("harness did not finish");
    let count = |key: &str| -> u64 {
        done.split_whitespace().find_map(|w| w.strip_prefix(key)).and_then(|v| v.parse().ok()).unwrap_or(0)
    };
    assert!(count("video=") >= 100, "{done}");
    assert!(count("audio=") >= 100, "{done}");
    assert!(done.contains("info=Caudal/Caudal/"), "{done}");
    assert!(text.contains(&format!("{W}x{H} codec=")), "no {W}x{H} pictures");
    println!("sent={} skipped={}", stats.video_sent.load(Ordering::Relaxed), stats.video_skipped.load(Ordering::Relaxed));
    handle.shutdown().await;
    feeder.abort();
}
