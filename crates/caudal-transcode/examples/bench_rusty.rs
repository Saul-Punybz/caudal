//! Benchmark for NOTES.md: the `RustyH264` engine's per-frame work (decode,
//! bilinear scale, encode) on an Annex B H.264 file without B-frames (one
//! slice per frame), writing the result as Annex B for PSNR with ffmpeg.
//!
//! `cargo run --release -p caudal-transcode --example bench_rusty -- in.h264 out.h264 360 1000`

#[path = "../src/scale.rs"]
#[allow(dead_code)]
mod scale;

use std::time::{Duration, Instant};

use rusty_h264::{Decoder, Encoder, EncoderConfig, Preset};

fn nals(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i..i + 3] == [0, 0, 1] {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(k, &b)| {
            let mut e = starts.get(k + 1).map_or(data.len(), |&n| n - 3);
            while e > b && data[e - 1] == 0 {
                e -= 1;
            }
            &data[b..e]
        })
        .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let input = std::fs::read(&args[1]).expect("input");
    let height: usize = args.get(3).map_or(360, |s| s.parse().unwrap());
    let kbps: u32 = args.get(4).map_or(1000, |s| s.parse().unwrap());

    let mut dec = Decoder::new();
    let mut enc: Option<Encoder> = None;
    let mut out = Vec::new();
    let (mut t_dec, mut t_scale, mut t_enc) = (Duration::ZERO, Duration::ZERO, Duration::ZERO);
    let mut au = Vec::new();
    let mut frames = 0u32;
    let wall = Instant::now();
    for nal in nals(&input) {
        au.extend_from_slice(&[0, 0, 0, 1]);
        au.extend_from_slice(nal);
        if !matches!(nal[0] & 0x1F, 1 | 5) {
            continue;
        }
        let t = Instant::now();
        let pic = dec.decode(&au).expect("decode");
        t_dec += t.elapsed();
        au.clear();
        let Some(pic) = pic else { continue };
        let t = Instant::now();
        let (w, h) = scale::fit(pic.width, pic.height, height);
        let small = scale::scale(&pic, w, h);
        t_scale += t.elapsed();
        let e = enc.get_or_insert_with(|| {
            let mut c = EncoderConfig::new(w, h);
            c.bitrate = kbps * 1000;
            c.framerate = 30.0;
            c.gop_size = 60;
            c.min_keyint = 1;
            c.scenecut = 0;
            c.lookahead = 0;
            c.bframes = 0;
            c.preset = Preset::Fast;
            c.level_idc = 40;
            Encoder::new(c).expect("encoder")
        });
        let t = Instant::now();
        if frames % 60 == 0 {
            e.request_keyframe();
        }
        out.extend_from_slice(&e.try_encode(&small).expect("encode"));
        t_enc += t.elapsed();
        frames += 1;
    }
    let wall = wall.elapsed();
    std::fs::write(&args[2], &out).expect("output");
    let per = |d: Duration| d.as_secs_f64() * 1000.0 / f64::from(frames.max(1));
    println!(
        "frames={frames} wall={:.2}s fps={:.1} decode={:.2}ms/f scale={:.2}ms/f encode={:.2}ms/f bytes={}",
        wall.as_secs_f64(),
        f64::from(frames) / wall.as_secs_f64(),
        per(t_dec),
        per(t_scale),
        per(t_enc),
        out.len()
    );
}
