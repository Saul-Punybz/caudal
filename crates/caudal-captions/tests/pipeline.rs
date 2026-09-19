//! The captions pipeline on a slow machine, without a model: a stand-in
//! recogniser that costs a fixed time per call whatever the audio length
//! (Whisper encodes a full 30 s window for every call), fed by a decoder
//! that falls behind and loses audio, so the stream arrives as many short
//! runs separated by gaps (each gap closes a short chunk).
//!
//! Seen on a 4-vCPU x86 CI runner (tiny model on CPU) before this test
//! existed: 13 chunks for ~18 s of speech where an M4 cuts 4, 5 of them
//! dropped, and a last-chunk RTF of 50.9 (a scrap of audio paying for a
//! full window). This test, before the fix: 15 calls on 0.51 s scraps
//! offered, 8 dropped.

use std::sync::Arc;
use std::time::{Duration, Instant};

use caudal_captions::whisper::{EngineError, Language, Transcript};
use caudal_captions::{Captions, CaptionsConfig, Recognizer, StreamRule};
use caudal_core::captions::CaptionSource;
use caudal_core::{AudioParams, BufferConfig, Codec, Frame, Registry, TrackId, TrackInfo};
use parking_lot::Mutex;

/// One inference call: how many samples it was given.
type Calls = Arc<Mutex<Vec<usize>>>;

struct FixedCost {
    cost: Duration,
    calls: Calls,
}

impl Recognizer for FixedCost {
    fn transcribe(&mut self, pcm: &[f32], language: &Language) -> Result<Transcript, EngineError> {
        std::thread::sleep(self.cost);
        self.calls.lock().push(pcm.len());
        let lang = match language {
            Language::Fixed(c) => c.clone(),
            Language::Auto => "es".into(),
        };
        Ok(Transcript { text: "hola mundo".into(), language: lang, no_speech_prob: 0.0, avg_logprob: -0.1 })
    }

    fn reset(&mut self) {}
}

/// AAC frames (48 kHz mono, 1024 samples each) of a steady tone: loud
/// enough to be speech to the chunker, with no pause for it to cut at.
fn tone_frames(n: usize) -> Vec<bytes::Bytes> {
    let pcm: Vec<f32> =
        (0..n * 1024 + 4096).map(|i| (i as f32 * 2.0 * std::f32::consts::PI * 440.0 / 48_000.0).sin() * 0.3).collect();
    let mut enc = rusty_aac::AacEncoder::new(rusty_aac::AacEncoderConfig { bitrate_bps: 64_000, ..Default::default() });
    enc.push_pcm(&pcm, 1, 48_000).unwrap();
    enc.finish();
    let mut out = Vec::new();
    while let Ok(p) = enc.next_packet() {
        out.push(bytes::Bytes::from(p.data));
    }
    out.truncate(n);
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn short_chunks_on_a_slow_engine_are_coalesced_not_dropped() {
    const COST: Duration = Duration::from_millis(1000);
    const RUNS: usize = 16;
    const RUN_FRAMES: usize = 24; // 512 ms of audio
    const GAP_FRAMES: usize = 15; // 320 ms lost between runs (> the 150 ms gap rule)

    let calls = Calls::default();
    let reg = Registry::new();
    let captions = {
        let calls = calls.clone();
        Captions::start_with(
            reg.clone(),
            CaptionsConfig {
                model_dir: "unused".into(),
                model: "fixed-cost".into(),
                threads: 1,
                device: caudal_captions::DeviceChoice::Cpu,
                max_streams: 1,
                min_display_ms: 1000,
                rules: vec![StreamRule { streams: vec!["slow".into()], language: Language::Fixed("es".into()) }],
            },
            Box::new(move |_| Ok(Box::new(FixedCost { cost: COST, calls }) as Box<dyn Recognizer>)),
        )
    };
    let t0 = Instant::now();
    while !captions.model_ready() {
        assert!(t0.elapsed() < Duration::from_secs(5), "stand-in did not load");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let p = reg.publish("slow", BufferConfig::default()).unwrap();
    p.set_tracks(vec![TrackInfo {
        id: TrackId(0),
        codec: Codec::Aac,
        timescale: 48_000,
        init: rusty_aac::audio_specific_config_bytes(48_000, 1).into(),
        lang: None,
        video: None,
        audio: Some(AudioParams { sample_rate: 48_000, channels: 1 }),
    }])
    .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    // RUNS short runs of audio, one every 100 ms of wall time (the decoder
    // catching up in bursts), each followed by a hole in the timestamps.
    let frames = tone_frames(RUNS * RUN_FRAMES);
    let mut ts = 0i64;
    for run in frames.chunks(RUN_FRAMES) {
        for d in run {
            p.push(Frame { track: TrackId(0), dts: ts, pts: ts, keyframe: true, data: d.clone() }).unwrap();
            ts += 1024;
        }
        ts += GAP_FRAMES as i64 * 1024;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let last_push = Instant::now();

    // All the audio sent is either transcribed or counted as dropped.
    let sent_s = (RUNS * RUN_FRAMES * 1024) as f64 / 48_000.0;
    let heard_s = || calls.lock().iter().sum::<usize>() as f64 / 16_000.0;
    let deadline = Duration::from_secs(20);
    loop {
        let m = &captions.metrics()[0];
        if heard_s() + m.dropped_audio_seconds >= sent_s * 0.9 || last_push.elapsed() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let done = last_push.elapsed();
    let m = captions.metrics()[0].clone();
    let calls = calls.lock().clone();
    eprintln!(
        "slow engine: {} calls {:?} (s), {:.2} s heard of {sent_s:.2} s sent, {done:?} after the last push; {m:?}",
        calls.len(),
        calls.iter().map(|n| format!("{:.2}", *n as f64 / 16_000.0)).collect::<Vec<_>>(),
        heard_s(),
    );

    assert_eq!(m.dropped_chunks, 0, "{m:?}");
    assert_eq!(m.dropped_audio_seconds, 0.0, "{m:?}");
    assert!(heard_s() >= sent_s * 0.9, "{:.2} s heard of {sent_s:.2} s", heard_s());
    // Fewer, fuller inferences: never one per short chunk.
    assert!(calls.len() <= RUNS / 3, "{} calls for {RUNS} short chunks", calls.len());
    // No call on a short scrap while more audio was on its way (the last
    // one may be the stream's tail).
    for n in &calls[..calls.len() - 1] {
        assert!(*n >= 16_000, "a {:.2} s call", *n as f64 / 16_000.0);
    }
    // Bounded latency: the backlog clears within a few inference calls of
    // the audio ending, not a queue's worth of stale jobs later.
    assert!(done < 5 * COST, "backlog cleared {done:?} after the last push");
    // RTF over everything (a fixed cost spread over longer calls), not the
    // last call's.
    let expected = calls.len() as f64 * COST.as_secs_f64() / heard_s();
    assert!((m.real_time_factor - expected).abs() < 0.1, "RTF {} vs {expected:.3}: {m:?}", m.real_time_factor);
    assert!(captions.cues("slow", i64::MIN, i64::MAX).len() >= calls.len());
    drop(p);
}
