//! OMT pull end to end on loopback: our own OMT `Sender` (no announce,
//! addressed as `omt://127.0.0.1:<port>`) sends a moving UYVY pattern and a
//! stereo tone; the pull decodes, encodes through ffmpeg and publishes an
//! H.264 + AAC stream. The ffmpeg tests are skipped when ffmpeg is not on
//! PATH.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use caudal_core::{BufferConfig, Codec, Event, Frame, Registry, StartAt, Subscriber, TrackInfo};
use caudal_omt::time::OMT_HZ;
use caudal_omt::{DropReason, PullConfig, PullHandle, Quality, start_pulls};
use open_media_transport::command::Tally;
use open_media_transport::sender::{Sender, SenderConfig, VideoParams};
use vmx_codec::{Frame as VmxFrame, PixelFormat};

const W: usize = 320;
const H: usize = 180;
const FPS: i64 = 30;
const RATE: i64 = 48_000;
const CHUNK: usize = (RATE / FPS) as usize;
const FRAME_US: i64 = 1_000_000 / FPS;

fn have_ffmpeg() -> bool {
    std::process::Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success())
}

fn init_logs() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}

fn ffmpeg_running(stream: &str) -> bool {
    std::process::Command::new("pgrep")
        .args(["-f", &format!("service_name={stream} pipe:1")])
        .output()
        .is_ok_and(|o| !o.stdout.is_empty())
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

fn pattern(i: i64) -> VmxFrame {
    let mut f = VmxFrame::new(W, H, PixelFormat::Uyvy);
    let bar = (i as usize * 4) % W;
    let data = &mut f.planes[0].data;
    for y in 0..H {
        for x in (0..W).step_by(2) {
            let luma = if (bar..bar + 8).contains(&x) { 235 } else { 16 + (x * 200 / W) as u8 };
            let o = (y * W + x) * 2;
            data[o..o + 4].copy_from_slice(&[128, luma, 128, luma]);
        }
    }
    f
}

/// One chunk of a 440 Hz tone from sample `start`, planar stereo.
fn tone(start: i64) -> Vec<f32> {
    let one: Vec<f32> = (0..CHUNK)
        .map(|k| 0.3 * (2.0 * std::f32::consts::PI * 440.0 * (start + k as i64) as f32 / RATE as f32).sin())
        .collect();
    [one.clone(), one].concat()
}

const PARAMS: VideoParams = VideoParams {
    frame_rate_n: FPS as i32,
    frame_rate_d: 1,
    aspect_ratio: 16.0 / 9.0,
    color_space: 709,
    premultiplied: false,
};

/// A loopback OMT source sending pattern + tone in real time from its own
/// thread, stamped from `t0`.
struct Source {
    sender: Arc<Sender>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Source {
    /// `port` = `None` picks a free one. With `wait_for_receiver`, nothing
    /// is sent until a receiver has subscribed to both video and audio, so
    /// the first frame of each is the first one sent.
    fn start(port: Option<u16>, t0: i64, wait_for_receiver: bool) -> Source {
        let ports = match port {
            Some(p) => p..=p,
            None => 17_100..=17_400,
        };
        let sender = Arc::new(
            Sender::new(SenderConfig { announce: false, ports, ..SenderConfig::new("caudal pull test") }).unwrap(),
        );
        let stop = Arc::new(AtomicBool::new(false));
        let (s, st) = (sender.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            if wait_for_receiver {
                while !st.load(Ordering::SeqCst) {
                    let peers = s.peer_stats();
                    if peers.iter().any(|p| p.video) && peers.iter().any(|p| p.audio) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            let start = Instant::now();
            let mut i: i64 = 0;
            while !st.load(Ordering::SeqCst) {
                let ts = t0 + i * OMT_HZ / FPS;
                s.send_video(&pattern(i), PARAMS, ts, b"").unwrap();
                s.send_audio(&tone(i * CHUNK as i64), 2, RATE as i32, ts, b"").unwrap();
                i += 1;
                let due = start + Duration::from_micros((i * FRAME_US) as u64);
                if let Some(d) = due.checked_duration_since(Instant::now()) {
                    std::thread::sleep(d);
                }
            }
        });
        Source { sender, stop, thread: Some(thread) }
    }

    fn port(&self) -> u16 {
        self.sender.port()
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn pull(stream: &str, port: u16) -> PullConfig {
    PullConfig {
        stream: stream.into(),
        source: format!("omt://127.0.0.1:{port}"),
        quality: Quality::Default,
        video_kbps: 300,
        audio_kbps: 64,
        ffmpeg: "ffmpeg".into(),
        directory: None,
    }
}

fn status(h: &PullHandle, stream: &str) -> Arc<caudal_omt::PullStats> {
    h.statuses().into_iter().find(|s| s.stream == stream).expect("pull status").stats
}

/// Frames until `enough` says stop or `timeout` passes.
async fn collect(sub: &mut Subscriber, timeout: Duration, mut enough: impl FnMut(&[Frame]) -> bool) -> Vec<Frame> {
    let mut frames = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    while !enough(&frames) {
        match tokio::time::timeout_at(deadline, sub.recv()).await {
            Err(_) => break,
            Ok(Event::Frame(f)) => frames.push((*f).clone()),
            Ok(Event::End) => break,
            Ok(Event::Lagged { .. }) => panic!("reader lagged"),
            Ok(Event::TracksChanged | Event::Cue(_)) => {}
        }
    }
    frames
}

fn split(frames: &[Frame], tracks: &[TrackInfo]) -> (TrackInfo, Vec<i64>, TrackInfo, Vec<i64>) {
    let v = tracks.iter().find(|t| t.codec == Codec::H264).expect("H.264 track").clone();
    let a = tracks.iter().find(|t| t.codec == Codec::Aac).expect("AAC track").clone();
    let vp = frames.iter().filter(|f| f.track == v.id).map(|f| f.pts).collect();
    let ad = frames.iter().filter(|f| f.track == a.id).map(|f| f.dts).collect();
    (v, vp, a, ad)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pattern_and_tone_become_h264_and_aac() {
    if !have_ffmpeg() {
        eprintln!("SKIP: ffmpeg not on PATH");
        return;
    }
    init_logs();
    let name = "omt_pull_basic";
    let src = Source::start(None, 987_654_321_000, true);
    let registry = Registry::new();
    let handle = start_pulls(registry.clone(), BufferConfig::default(), vec![pull(name, src.port())]);

    let stream = wait_for(Duration::from_secs(20), || registry.get(name).filter(|s| s.tracks().len() == 2))
        .await
        .expect("stream with two tracks");
    let tracks = stream.tracks();
    let mut sub = registry.subscribe(name, StartAt::Oldest).unwrap();
    let frames = collect(&mut sub, Duration::from_secs(15), |f| f.len() >= 150).await;
    let (video, vpts, audio, adts) = split(&frames, &tracks);

    let vp = video.video.unwrap();
    assert_eq!((vp.width, vp.height), (W as u32, H as u32));
    assert_eq!(video.timescale, 90_000);
    assert_eq!(audio.audio.unwrap().sample_rate, RATE as u32);
    assert!(vpts.len() >= 40, "only {} pictures", vpts.len());
    assert!(adts.len() >= 40, "only {} AAC frames", adts.len());
    assert!(vpts.windows(2).all(|w| w[1] > w[0]), "video pts not increasing: {vpts:?}");
    assert!(adts.windows(2).all(|w| w[1] > w[0]), "audio dts not increasing");
    // The sender stamps on a 1/30 s grid from its first frame: every
    // picture lands on it (±1 tick of rounding).
    for p in &vpts {
        let off = p.rem_euclid(3000);
        assert!(off <= 1 || off >= 2999, "picture pts {p} is off the frame grid");
    }
    // First picture and first AAC frame both start at the first frame the
    // sender sent (it waited for both subscriptions).
    let v0 = video.to_micros(vpts[0]);
    let a0 = audio.to_micros(adts[0]);
    eprintln!("first picture {v0} us, first AAC {a0} us, A/V offset {} us", v0 - a0);
    assert!(v0 < FRAME_US, "first picture at {v0} us: the first frames were lost");
    assert!((v0 - a0).abs() < FRAME_US, "A/V offset {} us", v0 - a0);

    // We are a viewer: program tally reaches the sender.
    let st = status(&handle, name);
    let want = Tally { preview: true, program: true };
    assert!(wait_for(Duration::from_secs(5), || (src.sender.tally() == want).then_some(())).await.is_some());
    assert_eq!(st.tally(), want);
    let s = st.snapshot();
    eprintln!("stats: {s:?}");
    assert!(s.connected && s.video_pushed >= 40 && s.audio_pushed >= 40 && s.bytes_in > 0);
    assert_eq!(s.publishes, 1);
    assert_eq!(st.dropped(DropReason::Decode), 0);

    handle.shutdown().await;
    assert!(wait_for(Duration::from_secs(2), || stream.is_ended().then_some(())).await.is_some(), "stream not ended");
    assert!(
        wait_for(Duration::from_secs(5), || (!ffmpeg_running(name)).then_some(())).await.is_some(),
        "ffmpeg still running"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sender_restart_is_bridged_on_the_same_stream() {
    if !have_ffmpeg() {
        eprintln!("SKIP: ffmpeg not on PATH");
        return;
    }
    init_logs();
    let name = "omt_pull_restart";
    let src = Source::start(None, 5_000_000_000, false);
    let port = src.port();
    let registry = Registry::new();
    let handle = start_pulls(registry.clone(), BufferConfig::default(), vec![pull(name, port)]);
    let stream = wait_for(Duration::from_secs(20), || registry.get(name).filter(|s| s.tracks().len() == 2))
        .await
        .expect("stream with two tracks");
    let tracks = stream.tracks();
    let mut sub = registry.subscribe(name, StartAt::LiveEdge).unwrap();
    let before = collect(&mut sub, Duration::from_secs(10), |f| f.len() >= 40).await;

    // Sender goes away for ~1.5 s and comes back on the same port with its
    // clock reset (timestamps jump backwards).
    drop(src);
    let gone = Instant::now();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let src = Source::start(Some(port), 0, false);
    let after = collect(&mut sub, Duration::from_secs(15), |f| f.len() >= 60).await;
    eprintln!("outage + recovery took {:?}", gone.elapsed());

    assert!(!stream.is_ended(), "stream ended during a short outage");
    assert!(Arc::ptr_eq(&stream, &registry.get(name).unwrap()), "stream was republished");
    let all: Vec<Frame> = before.into_iter().chain(after.iter().cloned()).collect();
    let (_, vpts, _, adts) = split(&all, &tracks);
    let (_, vafter, _, aafter) = split(&after, &tracks);
    assert!(vafter.len() >= 20 && aafter.len() >= 20, "{} pictures, {} AAC after restart", vafter.len(), aafter.len());
    assert!(vpts.windows(2).all(|w| w[1] > w[0]), "video pts not increasing across the restart");
    assert!(adts.windows(2).all(|w| w[1] > w[0]), "audio dts not increasing across the restart");
    let st = status(&handle, name).snapshot();
    eprintln!("stats: {st:?}");
    assert!(st.reconnects >= 1, "no reconnect counted");
    assert_eq!(st.publishes, 1);
    drop(src);
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_keeps_unchanged_pulls_and_restarts_changed_ones() {
    if !have_ffmpeg() {
        eprintln!("SKIP: ffmpeg not on PATH");
        return;
    }
    init_logs();
    let src = Source::start(None, 1_000, false);
    let port = src.port();
    let registry = Registry::new();
    let buffer = BufferConfig::default();
    let handle = start_pulls(registry.clone(), buffer, vec![pull("omt_reload_a", port)]);
    let a1 = wait_for(Duration::from_secs(20), || registry.get("omt_reload_a")).await.expect("a published");
    let a_stats = status(&handle, "omt_reload_a");

    // Same a, new b: a untouched.
    handle.reload(&registry, buffer, vec![pull("omt_reload_a", port), pull("omt_reload_b", port)]);
    let b = wait_for(Duration::from_secs(20), || registry.get("omt_reload_b")).await.expect("b published");
    assert!(Arc::ptr_eq(&a_stats, &status(&handle, "omt_reload_a")), "unchanged pull restarted");
    assert!(!a1.is_ended());

    // a changed, b removed: a restarted (its stream ends and comes back), b ends.
    let a2cfg = PullConfig { video_kbps: 200, ..pull("omt_reload_a", port) };
    handle.reload(&registry, buffer, vec![a2cfg]);
    assert!(wait_for(Duration::from_secs(2), || b.is_ended().then_some(())).await.is_some(), "b not ended");
    assert!(wait_for(Duration::from_secs(2), || a1.is_ended().then_some(())).await.is_some(), "old a not ended");
    let a2 = wait_for(Duration::from_secs(20), || registry.get("omt_reload_a").filter(|s| !Arc::ptr_eq(s, &a1)))
        .await
        .expect("a republished");
    assert!(!Arc::ptr_eq(&a_stats, &status(&handle, "omt_reload_a")));
    assert_eq!(handle.statuses().len(), 1);
    assert!(
        wait_for(Duration::from_secs(5), || (!ffmpeg_running("omt_reload_b")).then_some(())).await.is_some(),
        "b's ffmpeg still running"
    );

    // Stopping is prompt: the stream ends well within a second.
    let t = Instant::now();
    handle.stop();
    assert!(wait_for(Duration::from_secs(1), || a2.is_ended().then_some(())).await.is_some(), "a not ended");
    eprintln!("stream ended {:?} after stop()", t.elapsed());
    drop(src);
}

/// No ffmpeg needed: nothing listens, the pull keeps retrying, and stopping
/// it does not wait for the backoff.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreachable_source_retries_and_stops_promptly() {
    init_logs();
    let registry = Registry::new();
    // Port 9 (discard) on loopback: refused at once.
    let handle = start_pulls(registry.clone(), BufferConfig::default(), vec![pull("omt_nobody", 9)]);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let s = status(&handle, "omt_nobody").snapshot();
    assert!(!s.connected && s.publishes == 0);
    assert!(registry.get("omt_nobody").is_none());
    let t = Instant::now();
    handle.shutdown().await;
    assert!(t.elapsed() < Duration::from_millis(500), "shutdown took {:?}", t.elapsed());
}

/// `avcC` → Annex-B parameter sets, and the NAL length size.
fn avcc_params(init: &[u8]) -> (Vec<u8>, usize) {
    let len_size = usize::from(init[4] & 3) + 1;
    let mut out = Vec::new();
    let mut i = 5;
    for mask in [0x1f, 0xff] {
        let n = usize::from(init[i] & mask);
        i += 1;
        for _ in 0..n {
            let l = usize::from(u16::from_be_bytes([init[i], init[i + 1]]));
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&init[i + 2..i + 2 + l]);
            i += 2 + l;
        }
    }
    (out, len_size)
}

fn annex_b(frame: &[u8], len_size: usize, out: &mut Vec<u8>) {
    let mut i = 0;
    while i + len_size <= frame.len() {
        let l = frame[i..i + len_size].iter().fold(0usize, |a, &b| (a << 8) | usize::from(b));
        i += len_size;
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&frame[i..(i + l).min(frame.len())]);
        i += l;
    }
}

/// A raw AAC frame with an ADTS header built from the AudioSpecificConfig.
fn adts(asc: &[u8], au: &[u8], out: &mut Vec<u8>) {
    let obj = asc[0] >> 3;
    let fi = ((asc[0] & 7) << 1) | (asc[1] >> 7);
    let ch = (asc[1] >> 3) & 0xf;
    let fl = au.len() + 7;
    out.extend_from_slice(&[
        0xff,
        0xf1,
        ((obj - 1) << 6) | (fi << 2) | (ch >> 2),
        ((ch & 3) << 6) | (fl >> 11) as u8,
        (fl >> 3) as u8,
        (((fl & 7) as u8) << 5) | 0x1f,
        0xfc,
    ]);
    out.extend_from_slice(au);
}

/// External check, run by hand: pull a real OMT source (e.g. the libomtnet
/// harness, `send NAME SECONDS`) and write what Caudal publishes as
/// `pull.h264` (Annex-B) and `pull.aac` (ADTS) for ffprobe. See NOTES.md.
///
/// `CAUDAL_OMT_SOURCE=omt://127.0.0.1:6400 CAUDAL_OMT_OUT=dir cargo test -p caudal-omt --test pull -- --ignored external`
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs an external OMT source (CAUDAL_OMT_SOURCE)"]
async fn external_source_to_files() {
    init_logs();
    let source = std::env::var("CAUDAL_OMT_SOURCE").expect("CAUDAL_OMT_SOURCE");
    let out = std::path::PathBuf::from(std::env::var("CAUDAL_OMT_OUT").unwrap_or_else(|_| ".".into()));
    let seconds: u64 = std::env::var("CAUDAL_OMT_SECONDS").ok().and_then(|s| s.parse().ok()).unwrap_or(5);
    let name = "omt_external";
    let registry = Registry::new();
    let cfg = PullConfig { source, ..pull(name, 1) };
    let handle = start_pulls(registry.clone(), BufferConfig::default(), vec![cfg]);
    let stream = wait_for(Duration::from_secs(20), || registry.get(name).filter(|s| s.tracks().len() == 2))
        .await
        .expect("stream with two tracks");
    let tracks = stream.tracks();
    let mut sub = registry.subscribe(name, StartAt::Oldest).unwrap();
    let frames = collect(&mut sub, Duration::from_secs(seconds), |_| false).await;
    let st = status(&handle, name).snapshot();
    handle.shutdown().await;

    let (video, vpts, audio, adts_dts) = split(&frames, &tracks);
    let (mut h264, len_size) = avcc_params(&video.init);
    let mut aac = Vec::new();
    for f in &frames {
        if f.track == video.id {
            annex_b(&f.data, len_size, &mut h264);
        } else if f.track == audio.id {
            adts(&audio.init, &f.data, &mut aac);
        }
    }
    std::fs::write(out.join("pull.h264"), &h264).unwrap();
    std::fs::write(out.join("pull.aac"), &aac).unwrap();
    let span = |t: &TrackInfo, v: &[i64]| t.to_micros(*v.last().unwrap()) - t.to_micros(v[0]);
    eprintln!(
        "tracks: {:?} {:?} / {:?} {:?}\npictures {} over {} us, AAC frames {} over {} us, first picture {} us, first AAC {} us\nstats: {st:?}",
        video.codec,
        video.video,
        audio.codec,
        audio.audio,
        vpts.len(),
        span(&video, &vpts),
        adts_dts.len(),
        span(&audio, &adts_dts),
        video.to_micros(vpts[0]),
        audio.to_micros(adts_dts[0]),
    );
    assert!(vpts.windows(2).all(|w| w[1] > w[0]));
    assert!(adts_dts.windows(2).all(|w| w[1] > w[0]));
}
