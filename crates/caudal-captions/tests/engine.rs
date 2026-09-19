//! The recogniser on real speech, and the whole in-process pipeline
//! (AAC frames in a live stream → cues), against a real Whisper model.
//!
//! Needs `CAUDAL_WHISPER_MODELS` pointing at a model directory holding
//! `whisper-tiny/` (`caudal captions fetch-model tiny --dir <dir>`; CI
//! fetches it) and a text-to-speech tool to make the speech: macOS `say`
//! (voices Paulina and Samantha) or `espeak-ng` (CI). Without either the
//! tests print SKIP, which CI treats as a failure.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use caudal_captions::audio::Resampler;
use caudal_captions::chunk::{ChunkConfig, Chunker};
use caudal_captions::eval::{recall, wer};
use caudal_captions::whisper::{DeviceChoice, Language, Model};
use caudal_captions::{Captions, CaptionsConfig, StreamRule};
use caudal_core::captions::CaptionSource;
use caudal_core::{AudioParams, BufferConfig, Codec, Frame, Registry, TrackId, TrackInfo};

const ES: &str = "Buenas tardes y bienvenidos a la transmisión en vivo. Hoy vamos a hablar del clima en Puerto Rico durante la temporada de huracanes. Se esperan lluvias fuertes durante la noche del jueves. Les recomendamos preparar agua, comida y baterías para varios días.";
const EN: &str = "Good afternoon and welcome to the live broadcast. Today we will talk about the weather in Puerto Rico during the hurricane season. Heavy rain is expected during Thursday night. We recommend that you prepare water, food and batteries for several days.";

fn model_dir() -> Option<(PathBuf, String)> {
    let dir = PathBuf::from(std::env::var_os("CAUDAL_WHISPER_MODELS")?);
    let name = std::env::var("CAUDAL_WHISPER_MODEL").unwrap_or_else(|_| "tiny".into());
    caudal_captions::models::check(&dir, &name).ok()?;
    Some((dir, name))
}

fn have(tool: &str) -> bool {
    Command::new("which").arg(tool).output().is_ok_and(|o| o.status.success())
}

/// Which synthesiser made the speech: `say` is close to natural speech,
/// `espeak-ng` is robotic and gets a looser bar.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Voice {
    Say,
    Espeak,
}

/// Speaks `text` in `lang` into 16 kHz mono samples.
fn speak(lang: &str, text: &str) -> Option<(Vec<f32>, Voice)> {
    let dir = tempfile_dir();
    let wav = dir.join(format!("{lang}.wav"));
    // CAUDAL_TTS=espeak forces the CI voice on a Mac (it has both).
    let voice = if have("say") && std::env::var("CAUDAL_TTS").as_deref() != Ok("espeak") {
        let v = say_voice(lang);
        let ok = Command::new("say")
            .args(["-v", &v, "-o"])
            .arg(&wav)
            .args(["--data-format=LEI16@16000", text])
            .status()
            .is_ok_and(|s| s.success());
        ok.then_some(Voice::Say)?
    } else if have("espeak-ng") {
        let ok = Command::new("espeak-ng")
            .args(["-v", lang, "-s", "150", "-w"])
            .arg(&wav)
            .arg(text)
            .status()
            .is_ok_and(|s| s.success());
        ok.then_some(Voice::Espeak)?
    } else {
        return None;
    };
    let (pcm, rate) = caudal_captions::read_wav(&std::fs::read(&wav).ok()?)?;
    let _ = std::fs::remove_dir_all(&dir);
    let pcm = if rate == 16_000 { pcm } else { Resampler::new(rate, 16_000).process(&pcm) };
    Some((pcm, voice))
}

fn tempfile_dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "caudal-captions-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn ready() -> Option<(PathBuf, String)> {
    let Some(m) = model_dir() else {
        eprintln!("SKIP: no Whisper model (set CAUDAL_WHISPER_MODELS; `caudal captions fetch-model tiny --dir <dir>`)");
        return None;
    };
    if !have("say") && !have("espeak-ng") {
        eprintln!("SKIP: no text-to-speech tool (say or espeak-ng) to make test speech");
        return None;
    }
    Some(m)
}

#[test]
fn transcribes_spanish_and_english_speech_in_real_time() {
    let Some((dir, name)) = ready() else { return };
    let pool = rayon::ThreadPoolBuilder::new().num_threads(4).build().unwrap();
    let mut model =
        pool.install(|| Model::load(&caudal_captions::models::model_path(&dir, &name), DeviceChoice::Auto)).unwrap();
    for (lang, script) in [("es", ES), ("en", EN)] {
        let (pcm, voice) = speak(lang, script).expect("speech");
        let mut chunker = Chunker::new(ChunkConfig::default());
        let mut chunks = chunker.push(&pcm);
        chunks.extend(chunker.flush());
        assert!(chunks.len() >= 3, "{lang}: {} chunks", chunks.len());
        let t0 = Instant::now();
        let mut text = String::new();
        for c in &chunks {
            let t = pool.install(|| model.transcribe(&c.pcm, &Language::Fixed(lang.into()))).unwrap();
            assert_eq!(t.language, lang);
            if !t.is_silence() {
                text.push_str(&t.text);
                text.push(' ');
            }
        }
        let rtf = t0.elapsed().as_secs_f64() / (pcm.len() as f64 / 16_000.0);
        let w = wer(script, &text);
        eprintln!("{lang} ({voice:?}, {name}, {}): RTF {rtf:.3}, WER {:.1}%: {text}", model.device_name(), w * 100.0);
        let max_wer = if voice == Voice::Say { 0.25 } else { 0.6 };
        assert!(w <= max_wer, "{lang}: WER {w:.2} > {max_wer}: {text}");
        // Real time with room to spare, even in a debug test build (the
        // model crates are built optimised, see the workspace Cargo.toml).
        assert!(rtf < 0.8, "{lang}: RTF {rtf:.2}");

        // Language detection agrees on a whole chunk of speech.
        let detected = pool.install(|| model.transcribe(&chunks[1].pcm, &Language::Auto)).unwrap();
        assert_eq!(detected.language, lang, "{lang}: detected {detected:?}");
    }
    // Silence is not speech: nothing to caption.
    let t = pool.install(|| model.transcribe(&vec![0.0; 32_000], &Language::Fixed("es".into()))).unwrap();
    assert!(t.is_silence() || t.text.split_whitespace().count() <= 2, "{t:?}");
}

/// Speech encoded to AAC (48 kHz, like a real encoder), published into a
/// live stream at 4x real time, comes out as cues on the stream's timeline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_aac_stream_gets_cues() {
    let Some((dir, name)) = ready() else { return };
    let (pcm16, voice) = speak("es", ES).expect("speech");
    let pcm48 = {
        let mut r = Resampler::new(16_000, 48_000);
        let mut out = r.process(&pcm16);
        out.extend(r.process(&[0.0; 64]));
        out
    };
    let mut enc = rusty_aac::AacEncoder::new(rusty_aac::AacEncoderConfig { bitrate_bps: 64_000, ..Default::default() });
    enc.push_pcm(&pcm48, 1, 48_000).unwrap();
    enc.finish();
    let mut packets = Vec::new();
    while let Ok(p) = enc.next_packet() {
        packets.push(p);
    }
    assert!(packets.len() > 500, "{} AAC frames", packets.len());

    let reg = Registry::new();
    let captions = Captions::start(
        reg.clone(),
        CaptionsConfig {
            model_dir: dir,
            model: name,
            threads: 4,
            device: if std::env::var("CAUDAL_CAPTIONS_DEVICE").as_deref() == Ok("cpu") { DeviceChoice::Cpu } else { DeviceChoice::Auto },
            max_streams: 1,
            min_display_ms: 3000,
            rules: vec![StreamRule { streams: vec!["news*".into()], language: Language::Fixed("es".into()) }],
        },
    );
    let t0 = Instant::now();
    while !captions.model_ready() {
        assert!(t0.elapsed() < Duration::from_secs(60), "model did not load");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(captions.track("news-es").is_some() && captions.track("other").is_none());
    assert!(captions.track("news-es+480p").is_none(), "renditions share the source's captions");

    let p = reg.publish("news-es", BufferConfig::default()).unwrap();
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
    // Media starts at 100 s, so cue times must follow the stream's clock.
    let base = 100 * 48_000;
    let total_us = packets.len() as i64 * 1024 * 1_000_000 / 48_000;
    for (i, pk) in packets.iter().enumerate() {
        let ts = base + i as i64 * 1024;
        p.push(Frame { track: TrackId(0), dts: ts, pts: ts, keyframe: true, data: pk.data.clone().into() }).unwrap();
        if i % 8 == 7 {
            // 8 frames = 171 ms of audio, pushed every ~43 ms: 4x real time.
            tokio::time::sleep(Duration::from_millis(43)).await;
        }
    }
    // Keep the clock moving (silence) while the last chunks finish.
    let silence = {
        let mut e = rusty_aac::AacEncoder::default();
        e.push_pcm(&vec![0.0; 48_000 * 4], 1, 48_000).unwrap();
        e.finish();
        let mut v = Vec::new();
        while let Ok(p) = e.next_packet() {
            v.push(p.data);
        }
        v
    };
    for (j, d) in silence.iter().enumerate() {
        let ts = base + (packets.len() + j) as i64 * 1024;
        p.push(Frame { track: TrackId(0), dts: ts, pts: ts, keyframe: true, data: d.clone().into() }).unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let last_push = Instant::now();
    let t0 = Instant::now();
    let cues = loop {
        let cues = captions.cues("news-es", i64::MIN, i64::MAX);
        let text: String = cues.iter().map(|c| c.text.replace('\n', " ") + " ").collect();
        if recall("comida baterías varios días", &text) >= 0.75 || t0.elapsed() > Duration::from_secs(30) {
            break cues;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let text: String = cues.iter().map(|c| c.text.replace('\n', " ") + " ").collect();
    let r = recall(ES, &text);
    eprintln!("pipeline ({voice:?}): recall {:.0}%, {} cues: {text}", r * 100.0, cues.len());
    assert!(r >= if voice == Voice::Say { 0.8 } else { 0.5 }, "recall {r:.2}: {text}");
    // On the stream's clock, after the speech began, never before it; at
    // most the live edge when we read them: the media pushed (speech, then
    // 4 s of silence), plus the wall time since the last push (cues land at
    // the live edge once their text is ready, and the edge keeps moving),
    // plus the reading time of the cues before them. A fixed bound failed
    // on a 4-core CI runner where inference ran slower than real time.
    let start = 100_000_000;
    let live_edge = start + total_us + 4_000_000 + last_push.elapsed().as_micros() as i64;
    for c in &cues {
        assert!(c.start_us >= start && c.start_us <= live_edge + 5_000_000, "live edge {live_edge}: {c:?}");
        assert!(c.end_us > c.start_us && c.text.lines().count() <= 2, "{c:?}");
        assert!(c.text.lines().all(|l| l.chars().count() <= caudal_captions::cues::LINE), "{c:?}");
    }
    let m = &captions.metrics()[0];
    eprintln!("pipeline metrics: {m:?}");
    assert_eq!(m.stream, "news-es");
    assert!(m.chunks >= 3 && m.cues >= cues.len() as u64, "{m:?}");
    assert_eq!(m.dropped_chunks, 0, "{m:?}");
    assert!(m.real_time_factor > 0.0 && m.real_time_factor < 1.0, "{m:?}");
    drop(p);
}

/// A `say` voice for `lang`: Paulina (es_MX) / Samantha (en_US) when
/// installed, else the first installed voice of that language (CI runners
/// do not always have the same voices).
fn say_voice(lang: &str) -> String {
    let want = if lang == "es" { "Paulina" } else { "Samantha" };
    let list = Command::new("say").args(["-v", "?"]).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
    let list = list.unwrap_or_default();
    let installed: Vec<(&str, &str)> = list
        .lines()
        .filter_map(|l| {
            let (name, rest) = l.split_once("  ")?;
            Some((name.trim(), rest.split_whitespace().next()?))
        })
        .collect();
    if installed.iter().any(|(n, _)| *n == want) {
        return want.to_owned();
    }
    installed
        .iter()
        .find(|(_, locale)| locale.starts_with(&format!("{lang}_")))
        .map_or_else(|| want.to_owned(), |(n, _)| (*n).to_owned())
}
