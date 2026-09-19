//! Timestamps for a pulled stream.
//!
//! MoQ's Legacy container (what `caudal-moq` publishes) carries one
//! presentation timestamp in microseconds per frame and no decode
//! timestamp. Frames still arrive in decode order, so the edge authors a
//! DTS the way `moq-mux`'s FLV export does, `dts = max(prev + 1, pts -
//! reserve)`, with the reserve learned from the reordering it actually
//! sees (B-frames): after the first reordered frame DTS stays monotonic
//! and never above PTS. Streams without B-frames get `dts == pts`.
//!
//! [`Rebase`] keeps the timeline continuous when the edge fails over to
//! another origin, whose clock may start anywhere.

/// Clock of one pulled track: microseconds in, `(dts, pts)` in the track's
/// own timescale out.
#[derive(Debug, Clone)]
pub(crate) struct TrackClock {
    timescale: u32,
    video: bool,
    /// Ticks subtracted from PTS to get DTS; only grows.
    reserve: i64,
    last_dts: Option<i64>,
    max_pts: Option<i64>,
}

impl TrackClock {
    pub(crate) fn new(timescale: u32, video: bool) -> Self {
        Self { timescale: timescale.max(1), video, reserve: 0, last_dts: None, max_pts: None }
    }

    fn ticks(&self, micros: i64) -> i64 {
        let t = i128::from(self.timescale);
        // Round to nearest so 48 kHz audio lands on whole AAC frames.
        ((i128::from(micros) * t + 500_000).div_euclid(1_000_000)) as i64
    }

    /// `(dts, pts)` for a frame presented at `pts_us`.
    pub(crate) fn stamp(&mut self, pts_us: i64) -> (i64, i64) {
        let mut pts = self.ticks(pts_us);
        let mut dts = if self.video {
            if let Some(m) = self.max_pts
                && m > pts
            {
                // Reordered: this frame is presented before one decoded
                // earlier. One tick more than the gap keeps DTS below PTS.
                self.reserve = self.reserve.max(m - pts + 1);
            }
            self.max_pts = Some(self.max_pts.map_or(pts, |m| m.max(pts)));
            pts - self.reserve
        } else {
            pts
        };
        if let Some(l) = self.last_dts
            && dts <= l
        {
            dts = l + 1;
            if !self.video {
                pts = dts;
            } else if dts > pts {
                // Still not enough room below PTS (deeper reordering than
                // seen so far): widen the reserve for the frames to come.
                self.reserve += dts - pts;
            }
        }
        self.last_dts = Some(dts);
        (dts, pts)
    }
}

/// Offset added to every incoming timestamp so the local timeline keeps
/// going across a failover.
#[derive(Debug, Default)]
pub(crate) struct Rebase {
    offset_us: i64,
    /// Newest mapped timestamp handed out.
    high_us: Option<i64>,
    /// A new source started; the next frame fixes the offset.
    pending: bool,
}

/// Distance kept between the last frame of the old source and the first
/// of the new one.
const GAP_US: i64 = 50_000;

impl Rebase {
    /// The next frame comes from a new source (another origin, or the same
    /// one after a reconnect).
    pub(crate) fn new_source(&mut self) {
        self.pending = true;
    }

    pub(crate) fn map(&mut self, raw_us: i64) -> i64 {
        if self.pending {
            self.pending = false;
            if let Some(h) = self.high_us {
                self.offset_us = h + GAP_US - raw_us;
            }
        }
        let t = raw_us + self.offset_us;
        self.high_us = Some(self.high_us.map_or(t, |h| h.max(t)));
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: i64 = 40_000; // 25 fps in µs

    #[test]
    fn no_reordering_keeps_dts_equal_to_pts() {
        let mut c = TrackClock::new(90_000, true);
        for i in 0..10 {
            let (dts, pts) = c.stamp(i * D);
            assert_eq!(dts, pts);
            assert_eq!(pts, i * 3600);
        }
    }

    #[test]
    fn b_frames_converge_to_monotonic_dts_below_pts() {
        // I P B B P B B … in decode order, presentation 0 3 1 2 6 4 5 …
        let mut order = vec![0];
        for g in 0..30 {
            let base = g * 3;
            order.extend([base + 3, base + 1, base + 2]);
        }
        let mut c = TrackClock::new(90_000, true);
        let mut last = i64::MIN;
        let mut bad = 0;
        for (i, &n) in order.iter().enumerate() {
            let (dts, pts) = c.stamp(n * D);
            assert!(dts > last, "dts not increasing at {i}");
            last = dts;
            if dts > pts {
                bad += 1;
                assert!(i < 8, "dts above pts after convergence at frame {i}");
            }
        }
        assert!(bad <= 2, "{bad}");
    }

    #[test]
    fn audio_rounds_to_sample_clock_and_never_repeats() {
        let mut c = TrackClock::new(48_000, false);
        // AAC frames of 1024 samples at 48 kHz, as µs (21333.33… rounded).
        let mut last = -1;
        for i in 0..100i64 {
            let us = (i * 1024 * 1_000_000 + 24_000) / 48_000;
            let (dts, pts) = c.stamp(us);
            assert_eq!(dts, pts);
            assert_eq!(pts, i * 1024);
            assert!(dts > last);
            last = dts;
        }
    }

    #[test]
    fn rebase_continues_after_a_new_source() {
        let mut r = Rebase::default();
        assert_eq!(r.map(1_000_000), 1_000_000, "the first source keeps its clock");
        assert_eq!(r.map(1_040_000), 1_040_000);
        r.new_source();
        // The second origin's clock is far behind.
        assert_eq!(r.map(5_000), 1_040_000 + GAP_US);
        assert_eq!(r.map(45_000), 1_040_000 + GAP_US + 40_000);
        r.new_source();
        // …or far ahead.
        let t = r.map(900_000_000);
        assert_eq!(t, 1_040_000 + GAP_US + 40_000 + GAP_US);
    }
}
