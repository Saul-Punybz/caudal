//! Fuzzes `caudal-omt`'s `time::TimeMap`: OMT 100 ns timestamps from the
//! network (any sender can stamp anything: backwards, repeated, huge jumps,
//! `i64::MIN`, a sample rate of 0) mapped onto Caudal's track clocks.
//! Beyond "no panic / no overflow", it checks the promises the ffmpeg feed
//! relies on: video pts strictly increase, and audio chunks never overlap
//! (each starts at or after the previous one's end, on the same rate).
//!
//! Input layout: a sequence of 17-byte records `[op][ts: i64 LE][a: u32 LE]
//! [b: u32 LE]`. `op` even is a video frame (`a` = frame duration in 100 ns),
//! odd an audio chunk (`a` = samples, `b % 400_000` = sample rate, which
//! reaches past the 384 kHz cap; `op & 2` picks 48 kHz instead, so many
//! chunks share a rate).

#![no_main]

use caudal_omt::time::TimeMap;
use libfuzzer_sys::fuzz_target;

/// Far from saturation, where the invariants must hold exactly.
const SANE: i64 = i64::MAX / 4;

fuzz_target!(|data: &[u8]| {
    let mut m = TimeMap::new();
    let mut last_video: Option<i64> = None;
    let mut audio_end: Option<(u32, i64)> = None;
    for r in data.chunks_exact(17) {
        let op = r[0];
        let ts = i64::from_le_bytes(r[1..9].try_into().unwrap());
        let a = u32::from_le_bytes(r[9..13].try_into().unwrap());
        let b = u32::from_le_bytes(r[13..17].try_into().unwrap());
        if op % 2 == 0 {
            let pts = m.video(ts, i64::from(a));
            if let Some(l) = last_video {
                if l.abs() < SANE && pts.abs() < SANE {
                    assert!(pts > l, "video pts {pts} after {l}");
                }
            }
            last_video = Some(pts);
        } else {
            let rate = if op & 2 != 0 { 48_000 } else { b % 400_000 };
            if let Some(pts) = m.audio(ts, a, rate) {
                if let Some((r, end)) = audio_end {
                    if r == rate && end.abs() < SANE && pts.abs() < SANE {
                        assert!(pts >= end, "audio chunk at {pts} overlaps the previous one ending at {end}");
                    }
                }
                audio_end = Some((rate, pts.saturating_add(i64::from(a))));
            }
        }
    }
});
