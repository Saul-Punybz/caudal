//! OMT timestamps onto Caudal track clocks.
//!
//! OMT stamps every frame in 100 ns units on the sender's clock. Caudal
//! keeps each track on its own timescale: 90 kHz for video, the sample rate
//! for audio. [`TimeMap`] does that mapping for one source:
//!
//! - **Shared origin:** the first timestamp of either track is zero on
//!   both, so audio and video stay aligned exactly as the sender stamped
//!   them.
//! - **Video:** `(ts - origin) * 9 / 1000`, rounded to the nearest tick.
//! - **Audio:** counted in samples from an anchor (`anchor + samples so
//!   far`), so AAC gets gap-free timestamps even when the sender's stamps
//!   jitter. If the stamps say audio went missing (the count is more than
//!   [`AUDIO_SLACK_MS`] behind), the anchor moves forward; if the count
//!   runs more than that ahead of the stamps, the chunk is dropped
//!   ([`TimeMap::audio`] returns `None`) so the count never runs away.
//! - **Discontinuities:** a timestamp that goes backwards (or repeats) or
//!   jumps more than [`MAX_JUMP`] forward (sender restart, reconnect,
//!   clock change) re-anchors that track to "last output + one interval".
//!   If the other track already re-anchored and its offset puts this
//!   timestamp just after this track's last one, that offset is adopted
//!   instead, so both tracks land on the same new timeline.
//!
//! Outputs are strictly increasing per track (video) and gap-free,
//! non-overlapping (audio) for any input, which the `omt_time_map` fuzz
//! target checks.

/// OMT timestamp ticks per second (100 ns units).
pub const OMT_HZ: i64 = 10_000_000;
/// Caudal's video timescale.
pub const VIDEO_HZ: u32 = 90_000;
/// A forward step larger than this (in OMT ticks) is a discontinuity.
pub const MAX_JUMP: i64 = OMT_HZ;
/// Highest audio sample rate accepted (chunks above it are dropped).
pub const MAX_SAMPLE_RATE: u32 = 384_000;
/// How far the audio sample count may drift from the sender's stamps
/// before it is corrected.
pub const AUDIO_SLACK_MS: i64 = 10;

/// `v * num / den`, rounded half away from zero.
fn rescale(v: i128, num: i128, den: i128) -> i128 {
    let n = v * num;
    if (n >= 0) == (den > 0) { (n + den / 2) / den } else { (n - den / 2) / den }
}

fn sat(v: i128) -> i64 {
    v.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

/// Microseconds (a Caudal track time via `TrackInfo::to_micros`) to an OMT
/// timestamp, for outputs.
pub fn micros_to_omt(us: i64) -> i64 {
    us.saturating_mul(10)
}

#[derive(Debug, Clone, Copy, Default)]
struct Track {
    /// Added to `ts - origin` (OMT ticks) to get this track's timeline.
    shift: i128,
    /// Last input timestamp.
    last_ts: Option<i64>,
    /// Last timeline position (OMT ticks) this track mapped to.
    last_t: i128,
}

#[derive(Debug, Clone, Copy, Default)]
struct Audio {
    track: Track,
    rate: u32,
    /// Output pts (in `rate` ticks) of the next sample.
    next: i128,
}

/// The mapping state of one OMT source. See the module docs.
#[derive(Debug, Clone, Default)]
pub struct TimeMap {
    origin: Option<i64>,
    video: Track,
    audio: Audio,
    discontinuities: u64,
}

impl TimeMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many times either track was re-anchored.
    pub fn discontinuities(&self) -> u64 {
        self.discontinuities
    }

    /// The 90 kHz pts of a video frame stamped `ts`. `frame_100ns` is the
    /// nominal frame duration (from the OMT frame rate), used to re-anchor
    /// after a discontinuity; clamped to 1 tick ..= 1 s.
    pub fn video(&mut self, ts: i64, frame_100ns: i64) -> i64 {
        let origin = *self.origin.get_or_insert(ts);
        let rel = i128::from(ts) - i128::from(origin);
        let dur = i128::from(frame_100ns.clamp(1, OMT_HZ));
        if let Some(last) = self.video.last_ts {
            let d = i128::from(ts) - i128::from(last);
            if d <= 0 || d > i128::from(MAX_JUMP) {
                let adopt = rel + self.audio.track.shift;
                let plausible = self.audio.track.last_ts.is_some()
                    && adopt > self.video.last_t
                    && adopt - self.video.last_t <= i128::from(MAX_JUMP);
                self.video.shift = if plausible { self.audio.track.shift } else { self.video.last_t + dur - rel };
                self.discontinuities += 1;
            }
        }
        let mut t = rel + self.video.shift;
        // Rounding to 90 kHz must not produce a repeat either.
        let prev = rescale(self.video.last_t, 9, 1000);
        if self.video.last_ts.is_some() && rescale(t, 9, 1000) <= prev {
            t = self.video.last_t + 112; // 112 x 100 ns > one 90 kHz tick
            self.video.shift = t - rel;
        }
        self.video.last_ts = Some(ts);
        self.video.last_t = t;
        sat(rescale(t, 9, 1000))
    }

    /// The pts (in `sample_rate` ticks) of an audio chunk of `samples`
    /// samples per channel stamped `ts`, or `None` when the chunk must be
    /// dropped (rate 0 or above [`MAX_SAMPLE_RATE`], empty chunk, or the sample count is running ahead
    /// of the sender's stamps). A change of rate re-anchors the audio track.
    pub fn audio(&mut self, ts: i64, samples: u32, sample_rate: u32) -> Option<i64> {
        if sample_rate == 0 || sample_rate > MAX_SAMPLE_RATE || samples == 0 {
            return None;
        }
        let origin = *self.origin.get_or_insert(ts);
        let rel = i128::from(ts) - i128::from(origin);
        let rate = i128::from(sample_rate);
        let hz = i128::from(OMT_HZ);
        let a = &mut self.audio;
        let slack = rate * i128::from(AUDIO_SLACK_MS) / 1000;
        match a.track.last_ts {
            None => {
                a.rate = sample_rate;
                a.next = rescale(rel + a.track.shift, rate, hz);
            }
            Some(last) => {
                if a.rate != sample_rate {
                    // Continue at the same instant on the new clock, rounded
                    // up so a change and back never lands before the end.
                    let n = a.next * rate;
                    a.next = -(-n).div_euclid(i128::from(a.rate.max(1)));
                    a.rate = sample_rate;
                }
                let d = i128::from(ts) - i128::from(last);
                if d <= 0 || d > i128::from(MAX_JUMP) {
                    let end_t = rescale(a.next, hz, rate);
                    let adopt = rel + self.video.shift;
                    let plausible = self.video.last_ts.is_some()
                        && adopt > a.track.last_t
                        && adopt - a.track.last_t <= i128::from(MAX_JUMP);
                    a.track.shift = if plausible { self.video.shift } else { end_t - rel };
                    self.discontinuities += 1;
                    let stamped = rescale(rel + a.track.shift, rate, hz);
                    a.next = a.next.max(stamped);
                } else {
                    let stamped = rescale(rel + a.track.shift, rate, hz);
                    if stamped - a.next > slack {
                        a.next = stamped; // samples went missing: leave a gap
                    } else if a.next - stamped > slack {
                        return None; // counting ahead of the sender: drop
                    }
                }
            }
        }
        let pts = a.next;
        // Unclamped: with the rate capped, i128 has room for any input.
        a.next += i128::from(samples);
        a.track.last_ts = Some(ts);
        a.track.last_t = rel + a.track.shift;
        Some(sat(pts))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const F60: i64 = OMT_HZ * 1001 / 60_000; // 59.94 fps
    const F30: i64 = OMT_HZ / 30;

    #[test]
    fn video_is_90khz_from_the_shared_origin() {
        let mut m = TimeMap::new();
        let t0 = 123_456_789_000;
        assert_eq!(m.video(t0, F30), 0);
        assert_eq!(m.video(t0 + F30, F30), 3000);
        assert_eq!(m.video(t0 + 2 * F30, F30), 6000);
        // 59.94: 1501.5 ticks per frame, rounded.
        let mut m = TimeMap::new();
        let pts: Vec<i64> = (0..4).map(|i| m.video(i * F60, F60)).collect();
        assert_eq!(pts, [0, 1501, 3003, 4504]);
    }

    #[test]
    fn audio_counts_samples_from_the_shared_origin() {
        let mut m = TimeMap::new();
        let t0 = 5_000_000;
        assert_eq!(m.video(t0, F30), 0);
        // Audio stamped 20 ms after the first video frame.
        let a0 = t0 + 200_000;
        assert_eq!(m.audio(a0, 960, 48_000), Some(960));
        // Stamps jitter by 0.3 ms; the count doesn't.
        assert_eq!(m.audio(a0 + 200_000 + 3_000, 960, 48_000), Some(1920));
        assert_eq!(m.audio(a0 + 400_000 - 3_000, 960, 48_000), Some(2880));
    }

    #[test]
    fn audio_before_video_sets_the_origin() {
        let mut m = TimeMap::new();
        assert_eq!(m.audio(1_000_000, 1024, 48_000), Some(0));
        // Video 50 ms later: 4500 ticks.
        assert_eq!(m.video(1_500_000, F30), 4500);
    }

    #[test]
    fn missing_audio_leaves_a_gap() {
        let mut m = TimeMap::new();
        // 480 samples at 48 kHz = 10 ms = 100_000 OMT ticks.
        assert_eq!(m.audio(0, 480, 48_000), Some(0));
        // Next chunk stamped 50 ms later (4 chunks lost): the gap shows.
        assert_eq!(m.audio(500_000, 480, 48_000), Some(2400));
        assert_eq!(m.audio(600_000, 480, 48_000), Some(2880));
    }

    #[test]
    fn counting_ahead_of_the_stamps_drops_a_chunk() {
        let mut m = TimeMap::new();
        // 10 ms chunks stamped only 8 ms apart: the count gains 2 ms each.
        let got: Vec<Option<i64>> = (0..8).map(|i| m.audio(i * 80_000, 480, 48_000)).collect();
        let want: Vec<Option<i64>> =
            vec![Some(0), Some(480), Some(960), Some(1440), Some(1920), Some(2400), None, Some(2880)];
        assert_eq!(got, want);
    }

    #[test]
    fn backwards_jump_reanchors_both_tracks_together() {
        let mut m = TimeMap::new();
        for i in 0..10 {
            m.video(i * F30, F30);
            m.audio(i * F30, 1600, 48_000);
        }
        let (v_last, a_next) = (27_000, 16_000);
        // Sender restarts: its clock goes back to 0.
        let v = m.video(1_000, F30);
        assert_eq!(v, v_last + 3000); // last + one frame
        let a = m.audio(1_000, 1600, 48_000).unwrap();
        // Audio adopts video's new offset: same instant, no overlap.
        assert!(a >= a_next);
        let a_us = a * 1_000_000 / 48_000;
        let v_us = v * 1_000_000 / 90_000;
        assert!((a_us - v_us).abs() < 1_000, "audio {a_us} us vs video {v_us} us");
        assert_eq!(m.discontinuities(), 2);
    }

    #[test]
    fn forward_jump_over_a_second_reanchors() {
        let mut m = TimeMap::new();
        m.video(0, F30);
        m.video(F30, F30);
        let v = m.video(F30 + 3600 * OMT_HZ, F30);
        assert_eq!(v, 6000);
        // A normal step afterwards continues from there.
        assert_eq!(m.video(2 * F30 + 3600 * OMT_HZ, F30), 9000);
    }

    #[test]
    fn repeated_timestamp_still_moves_forward() {
        let mut m = TimeMap::new();
        assert_eq!(m.video(0, F30), 0);
        assert_eq!(m.video(0, F30), 3000);
        assert_eq!(m.video(1, F30), 3000 + 1); // 100 ns later: rounded up to one tick
    }

    #[test]
    fn rate_change_continues_at_the_same_instant() {
        let mut m = TimeMap::new();
        assert_eq!(m.audio(0, 480, 48_000), Some(0));
        assert_eq!(m.audio(100_000, 441, 44_100), Some(441));
    }

    #[test]
    fn rate_change_and_back_never_overlaps() {
        let mut m = TimeMap::new();
        assert_eq!(m.audio(0, 7, 48_000), Some(0));
        // 7 samples at 48 kHz = 6.43 at 44.1 kHz: rounded up to 7.
        assert_eq!(m.audio(1_459, 1, 44_100), Some(7));
        // 8 at 44.1 kHz = 8.71 at 48 kHz: rounded up to 9, not 8.
        assert_eq!(m.audio(1_640, 1, 48_000), Some(9));
    }

    #[test]
    fn extremes_do_not_overflow() {
        let mut m = TimeMap::new();
        m.video(i64::MIN, 0);
        m.video(i64::MAX, i64::MAX);
        m.video(i64::MIN, i64::MIN);
        assert!(m.audio(i64::MAX, u32::MAX, 1).is_some());
        assert!(m.audio(i64::MIN, u32::MAX, MAX_SAMPLE_RATE).is_some());
        assert_eq!(m.audio(0, 1, MAX_SAMPLE_RATE + 1), None);
        assert_eq!(micros_to_omt(i64::MAX), i64::MAX);
        assert_eq!(micros_to_omt(1_500), 15_000);
    }
}
