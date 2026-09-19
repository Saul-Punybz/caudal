//! Measures the recogniser the way the live pipeline uses it: a 16 kHz
//! mono WAV is cut into chunks at pauses (as live audio would be) and each
//! chunk is transcribed on a pool capped at `--threads`. Prints the text,
//! the real-time factor (inference time / audio time; below 1 keeps up)
//! and, given the script, the word error rate.
//!
//! ```text
//! cargo run --release -p caudal-captions --example transcribe -- \
//!     <model_dir> <speech.wav> [--script speech.txt] [--language es|en|auto] \
//!     [--threads 4] [--device auto|cpu|metal] [--short-window] \
//!     [--min-ms 1500] [--max-ms 5000] [--pause-ms 300]
//! ```

use std::path::PathBuf;
use std::time::Instant;

use caudal_captions::chunk::{ChunkConfig, Chunker};
use caudal_captions::whisper::{DeviceChoice, Language, Model};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let positional: Vec<&String> = {
        let mut out = Vec::new();
        let mut skip = false;
        for a in &args {
            if skip {
                skip = false;
            } else if a.starts_with("--") {
                skip = a != "--short-window";
            } else {
                out.push(a);
            }
        }
        out
    };
    let (Some(dir), Some(wav)) = (positional.first(), positional.get(1)) else {
        eprintln!(
            "usage: transcribe <model_dir> <speech.wav> [--script f] [--language es|en|auto] [--threads n] [--device auto|cpu|metal] [--short-window]"
        );
        std::process::exit(2);
    };
    let threads: usize = flag("--threads").and_then(|t| t.parse().ok()).unwrap_or(4);
    let device = match flag("--device").as_deref() {
        Some("cpu") => DeviceChoice::Cpu,
        Some("metal") => DeviceChoice::Metal,
        _ => DeviceChoice::Auto,
    };
    let language = match flag("--language").as_deref() {
        None | Some("auto") => Language::Auto,
        Some(l) => Language::Fixed(l.to_owned()),
    };
    let (pcm, rate) = caudal_captions::read_wav(&std::fs::read(wav).expect("read wav")).expect("16-bit PCM WAV");
    assert_eq!(rate, 16_000, "resample to 16 kHz first (say --data-format=LEI16@16000)");

    let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().unwrap();
    let t0 = Instant::now();
    let mut model = Model::load(&PathBuf::from(dir.as_str()), device).expect("load model");
    model.full_window = !args.iter().any(|a| a == "--short-window");
    let load = t0.elapsed();

    let mut chunking = ChunkConfig::default();
    let ms = |name: &str, d: u32| flag(name).and_then(|v| v.parse().ok()).unwrap_or(d);
    chunking.min_ms = ms("--min-ms", chunking.min_ms);
    chunking.max_ms = ms("--max-ms", chunking.max_ms);
    chunking.pause_ms = ms("--pause-ms", chunking.pause_ms);
    let mut chunker = Chunker::new(chunking);
    let mut chunks = chunker.push(&pcm);
    chunks.extend(chunker.flush());
    let audio_s = pcm.len() as f64 / 16_000.0;
    let mut spent = 0.0;
    let mut worst: f64 = 0.0;
    let mut text = String::new();
    // Warm-up (Metal compiles its kernels on first use).
    pool.install(|| model.transcribe(&pcm[..16_000.min(pcm.len())], &Language::Fixed("en".into()))).ok();
    for c in &chunks {
        let t = Instant::now();
        let out = pool.install(|| model.transcribe(&c.pcm, &language)).expect("transcribe");
        let dt = t.elapsed().as_secs_f64();
        let len = c.pcm.len() as f64 / 16_000.0;
        spent += dt;
        worst = worst.max(dt / len);
        println!(
            "[{:6.2}s +{:4.2}s] {:5.0} ms  lang={} no_speech={:.2} logprob={:.2}  {}",
            c.start as f64 / 16_000.0,
            len,
            dt * 1000.0,
            out.language,
            out.no_speech_prob,
            out.avg_logprob,
            out.text
        );
        if !out.is_silence() {
            text.push_str(&out.text);
            text.push(' ');
        }
    }
    println!("---");
    println!(
        "model={} device={} threads={threads} window={} load={:.2}s chunks={} audio={audio_s:.1}s inference={spent:.2}s",
        dir,
        model.device_name(),
        if model.full_window { "30s" } else { "short" },
        load.as_secs_f64(),
        chunks.len()
    );
    println!("RTF={:.3} (worst chunk {:.3})", spent / audio_s, worst);
    if let Some(script) = flag("--script") {
        let script = std::fs::read_to_string(script).expect("read script");
        let wer = caudal_captions::eval::wer(&script, &text);
        println!("WER={:.1}% word accuracy={:.1}%", wer * 100.0, (1.0 - wer).max(0.0) * 100.0);
    }
}
