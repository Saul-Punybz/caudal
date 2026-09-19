//! Send pacing: multicast has no congestion control and switches drop
//! bursts (a keyframe muxed at once can be a few hundred datagrams), so
//! every datagram gets a send time instead of going out the moment the
//! muxer produces it.
//!
//! Two clocks combine:
//!
//! 1. **The stream's own clock.** Each frame's DTS maps to a wall-clock
//!    deadline (`anchor_wall + (dts - anchor_dts)`), so output leaves at
//!    the rate the media was produced, not the rate frames reach this task
//!    (a subscriber joining at the live edge gets the whole GOP since the
//!    last keyframe in one go; without this it would go out as one burst).
//!    A deadline more than [`REANCHOR_LATE`] behind wall time (the source
//!    stalled) or [`REANCHOR_EARLY`] ahead of it (a timestamp jump)
//!    re-anchors on the current frame instead of bursting or stalling.
//! 2. **A small constant-rate smoother.** Datagrams sharing one deadline
//!    (one frame's worth) are spread at [`HEADROOM`] times the stream's
//!    average bitrate, measured over the last [`RATE_WINDOW_US`] of media
//!    time. A keyframe several times the average frame size then drains
//!    over a few frame intervals instead of in one burst; with the rate
//!    above the average, the backlog always recovers. Never more than
//!    [`MAX_LAG`] behind a deadline: past that, the send time is clamped
//!    (a burst, counted as pacing lag, beats unbounded latency).
//!
//! Everything takes `now` as a parameter, so the math is unit-tested
//! without sleeping.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Media time over which the average bitrate is measured.
pub(crate) const RATE_WINDOW_US: i64 = 1_000_000;
/// No rate estimate until the window spans at least this much media time;
/// until then datagrams go out at their frame's deadline.
const MIN_WINDOW_US: i64 = 200_000;
/// Smoothing rate as a multiple of the measured average.
pub(crate) const HEADROOM: f64 = 1.5;
pub(crate) const REANCHOR_LATE: Duration = Duration::from_millis(500);
pub(crate) const REANCHOR_EARLY: Duration = Duration::from_secs(2);
pub(crate) const MAX_LAG: Duration = Duration::from_secs(1);

pub(crate) struct Pacer {
    enabled: bool,
    /// (wall clock, media time in µs) of the frame the timeline is pinned
    /// to.
    anchor: Option<(Instant, i64)>,
    /// Latest media time seen: deadlines never go backwards, even when an
    /// audio frame's DTS is a few ms behind the previous video frame's.
    last_media_us: i64,
    /// (media time µs, bytes) per muxed frame, oldest first.
    window: VecDeque<(i64, usize)>,
    window_bytes: usize,
    /// Earliest time the next datagram may go out under the rate limit.
    next_free: Option<Instant>,
}

impl Pacer {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            anchor: None,
            last_media_us: i64::MIN,
            window: VecDeque::new(),
            window_bytes: 0,
            next_free: None,
        }
    }

    /// The wall-clock deadline for output muxed from a frame at `media_us`
    /// (re-anchoring if the media clock and wall clock have drifted apart
    /// beyond what pacing should absorb).
    pub(crate) fn deadline(&mut self, now: Instant, media_us: i64) -> Instant {
        if !self.enabled {
            return now;
        }
        let media_us = media_us.max(self.last_media_us);
        self.last_media_us = media_us;
        if let Some((wall, anchor_us)) = self.anchor {
            let offset = media_us - anchor_us;
            let d = if offset >= 0 {
                wall.checked_add(Duration::from_micros(offset as u64))
            } else {
                wall.checked_sub(Duration::from_micros(offset.unsigned_abs()))
            };
            if let Some(d) = d
                && d + REANCHOR_LATE >= now
                && d <= now + REANCHOR_EARLY
            {
                return d;
            }
        }
        self.anchor = Some((now, media_us));
        self.window.clear();
        self.window_bytes = 0;
        now
    }

    /// Records `bytes` of output for the frame at `media_us`, feeding the
    /// average-bitrate estimate.
    pub(crate) fn record(&mut self, media_us: i64, bytes: usize) {
        if bytes == 0 {
            return;
        }
        self.window.push_back((media_us, bytes));
        self.window_bytes += bytes;
        while let Some(&(front, b)) = self.window.front() {
            if media_us - front <= RATE_WINDOW_US {
                break;
            }
            self.window.pop_front();
            self.window_bytes -= b;
        }
    }

    /// Average output rate in bytes per second over the window, or `None`
    /// while the window is too short to say.
    pub(crate) fn average_rate(&self) -> Option<f64> {
        let (&(first, first_bytes), &(last, _)) = (self.window.front()?, self.window.back()?);
        let span = last - first;
        if span < MIN_WINDOW_US {
            return None;
        }
        // The first entry's bytes belong to the instant the span starts at.
        Some((self.window_bytes - first_bytes) as f64 / (span as f64 / 1e6))
    }

    /// The send time for one datagram of `bytes` whose frame's deadline is
    /// `deadline`: no earlier than the deadline, no closer to the previous
    /// datagram than the smoothing rate allows, never more than
    /// [`MAX_LAG`] after the deadline.
    pub(crate) fn schedule(&mut self, deadline: Instant, bytes: usize) -> Instant {
        if !self.enabled {
            return deadline;
        }
        let at = self.next_free.map_or(deadline, |free| free.max(deadline)).min(deadline + MAX_LAG);
        let gap = self
            .average_rate()
            .map_or(Duration::ZERO, |rate| Duration::from_secs_f64(bytes as f64 / (rate * HEADROOM)));
        self.next_free = Some(at + gap);
        at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = 1000;

    #[test]
    fn deadlines_follow_the_media_clock() {
        let t0 = Instant::now();
        let mut p = Pacer::new(true);
        assert_eq!(p.deadline(t0, 5_000 * MS), t0, "first frame anchors at now");
        // A backlog delivered all at once (live-edge join) still gets
        // deadlines spaced by its own timestamps.
        assert_eq!(p.deadline(t0, 5_040 * MS), t0 + Duration::from_millis(40));
        assert_eq!(p.deadline(t0, 5_400 * MS), t0 + Duration::from_millis(400));
    }

    #[test]
    fn deadlines_never_go_backwards() {
        let t0 = Instant::now();
        let mut p = Pacer::new(true);
        p.deadline(t0, 0);
        assert_eq!(p.deadline(t0, 100 * MS), t0 + Duration::from_millis(100));
        // Audio 20 ms behind the last video frame.
        assert_eq!(p.deadline(t0, 80 * MS), t0 + Duration::from_millis(100));
    }

    #[test]
    fn a_stalled_source_reanchors_instead_of_bursting() {
        let t0 = Instant::now();
        let mut p = Pacer::new(true);
        p.deadline(t0, 0);
        // The source stalled for 3 s and resumes at media time 100 ms: its
        // deadline (t0+100ms) is 2.9 s in the past.
        let now = t0 + Duration::from_secs(3);
        assert_eq!(p.deadline(now, 100 * MS), now);
        assert_eq!(p.deadline(now, 140 * MS), now + Duration::from_millis(40));
    }

    #[test]
    fn a_small_lateness_is_absorbed_not_reanchored() {
        let t0 = Instant::now();
        let mut p = Pacer::new(true);
        p.deadline(t0, 0);
        let now = t0 + Duration::from_millis(300);
        assert_eq!(p.deadline(now, 100 * MS), t0 + Duration::from_millis(100));
    }

    #[test]
    fn a_timestamp_jump_forward_reanchors() {
        let t0 = Instant::now();
        let mut p = Pacer::new(true);
        p.deadline(t0, 0);
        assert_eq!(p.deadline(t0, 3_600_000 * MS), t0, "an hour ahead: re-anchor, don't wait an hour");
    }

    #[test]
    fn average_rate_over_the_media_window() {
        let mut p = Pacer::new(true);
        assert!(p.average_rate().is_none());
        // 25 fps, 5000 bytes a frame = 125 kB/s.
        for i in 0..50 {
            p.record(i * 40 * MS, 5000);
        }
        let rate = p.average_rate().unwrap();
        assert!((rate - 125_000.0).abs() < 1.0, "{rate}");
        // The window only keeps the last second.
        assert!(p.window.front().unwrap().0 >= 49 * 40 * MS - RATE_WINDOW_US);
    }

    #[test]
    fn a_short_window_has_no_estimate() {
        let mut p = Pacer::new(true);
        p.record(0, 5000);
        p.record(100 * MS, 5000);
        assert!(p.average_rate().is_none(), "100 ms is too short to estimate a bitrate");
    }

    #[test]
    fn datagrams_of_one_frame_are_spread_at_headroom_rate() {
        let t0 = Instant::now();
        let mut p = Pacer::new(true);
        for i in 0..25 {
            p.record(i * 40 * MS, 13_160); // 10 datagrams a frame: 329 kB/s
        }
        let rate = p.average_rate().unwrap();
        let gap = Duration::from_secs_f64(1316.0 / (rate * HEADROOM));
        let a = p.schedule(t0, 1316);
        let b = p.schedule(t0, 1316);
        let c = p.schedule(t0, 1316);
        assert_eq!(a, t0);
        assert_eq!(b, t0 + gap);
        assert_eq!(c, t0 + gap * 2);
        // ~2.67 ms apart: a 10-datagram frame spreads over ~27 ms of its
        // 40 ms slot instead of leaving in one burst.
        assert!(gap > Duration::from_millis(2) && gap < Duration::from_millis(3), "{gap:?}");
    }

    #[test]
    fn a_later_deadline_wins_over_the_rate_limit() {
        let t0 = Instant::now();
        let mut p = Pacer::new(true);
        for i in 0..25 {
            p.record(i * 40 * MS, 1316);
        }
        p.schedule(t0, 1316);
        let later = t0 + Duration::from_millis(40);
        assert_eq!(p.schedule(later, 1316), later, "an idle link sends at the frame's own deadline");
    }

    #[test]
    fn lag_is_capped() {
        let t0 = Instant::now();
        let mut p = Pacer::new(true);
        // A tiny average rate (1316 B/s) and a 100-datagram keyframe.
        p.record(0, 1316);
        p.record(1_000 * MS, 1316);
        let mut last = t0;
        for _ in 0..100 {
            last = p.schedule(t0, 1316);
        }
        assert_eq!(last, t0 + MAX_LAG, "never more than MAX_LAG behind the deadline");
    }

    #[test]
    fn disabled_pacing_sends_immediately() {
        let t0 = Instant::now();
        let mut p = Pacer::new(false);
        let d = p.deadline(t0, 999_999 * MS);
        assert_eq!(d, t0);
        p.record(0, 1_000_000);
        p.record(1_000 * MS, 1_000_000);
        assert_eq!(p.schedule(d, 1316), t0);
        assert_eq!(p.schedule(d, 1316), t0);
    }
}
