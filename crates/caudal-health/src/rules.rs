//! Pure rule logic: no registry, no I/O, no wall clock of its own. Driven by
//! whoever owns the clock (the watcher in production, a manual `Instant` in
//! tests), so hysteresis can be unit tested with `tokio::time::pause`
//! without a real stream or network.
//!
//! Each rule is a two-stage state machine:
//! 1. A tracker turns raw observations (a keyframe arrived, a byte count)
//!    into `is_bad: bool` for the current instant.
//! 2. [`RuleState::tick`] turns `is_bad` into at most one [`Transition`] per
//!    state change: `Alert` the instant a rule turns bad, `Resolved` only
//!    after it has been continuously good for `min_hold`. Entering bad never
//!    debounces (the duration is already baked into `is_bad`, e.g.
//!    `no_keyframe_secs`); only the way back debounces, so a value bouncing
//!    around a threshold does not flap a stream of resolved/alert pairs.

use std::time::Duration;

use tokio::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuleKind {
    NoKeyframe,
    MinBitrate,
    PublisherLost,
    NoAudio,
}

impl RuleKind {
    /// Lowercase name used in webhook payloads, the alerts API and metrics.
    pub fn as_str(self) -> &'static str {
        match self {
            RuleKind::NoKeyframe => "no_keyframe",
            RuleKind::MinBitrate => "min_bitrate",
            RuleKind::PublisherLost => "publisher_lost",
            RuleKind::NoAudio => "no_audio",
        }
    }

    pub const ALL: [RuleKind; 4] =
        [RuleKind::NoKeyframe, RuleKind::MinBitrate, RuleKind::PublisherLost, RuleKind::NoAudio];
}

/// What a tracker reports for the current tick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Observation {
    pub is_bad: bool,
    /// The measured value, in the rule's own unit (seconds, kbps).
    pub value: f64,
    /// The configured threshold, same unit, for the webhook payload.
    pub threshold: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Transition {
    Alert { value: f64, threshold: f64 },
    Resolved { value: f64, threshold: f64 },
}

/// Hysteresis for one rule on one stream.
#[derive(Debug, Clone, Copy, Default)]
pub struct RuleState {
    active: bool,
    /// First instant this tick the condition was observed good again, while
    /// still active. Cleared the moment it goes bad again.
    good_since: Option<Instant>,
}

impl RuleState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Advances the state machine one tick. `min_hold` is how long the
    /// condition must stay good, continuously, before a `Resolved` fires.
    pub fn tick(&mut self, now: Instant, obs: Observation, min_hold: Duration) -> Option<Transition> {
        if obs.is_bad {
            self.good_since = None;
            if self.active {
                return None;
            }
            self.active = true;
            Some(Transition::Alert { value: obs.value, threshold: obs.threshold })
        } else {
            if !self.active {
                return None;
            }
            match self.good_since {
                None => {
                    self.good_since = Some(now);
                    None
                }
                Some(since) if now.saturating_duration_since(since) >= min_hold => {
                    self.active = false;
                    self.good_since = None;
                    Some(Transition::Resolved { value: obs.value, threshold: obs.threshold })
                }
                Some(_) => None,
            }
        }
    }
}

/// "How long since the last time X happened", turned into an `Observation`
/// against a threshold. Used for no-keyframe, no-audio and publisher-lost:
/// all three are "nothing has happened for N seconds" rules.
#[derive(Debug, Clone, Copy)]
pub struct SinceTracker {
    last: Option<Instant>,
}

impl SinceTracker {
    /// Seeds the clock at `start` (stream creation, or the moment the
    /// publisher went away) rather than leaving it unset, so a stream that
    /// simply hasn't sent its first keyframe yet is judged against how long
    /// it has been trying, not treated as infinitely stale on tick one.
    pub fn seeded(start: Instant) -> Self {
        Self { last: Some(start) }
    }

    pub fn unset() -> Self {
        Self { last: None }
    }

    pub fn mark(&mut self, at: Instant) {
        self.last = Some(at);
    }

    pub fn observe(&self, now: Instant, threshold: Duration) -> Observation {
        let elapsed = match self.last {
            Some(last) => now.saturating_duration_since(last),
            None => Duration::MAX,
        };
        Observation { is_bad: elapsed >= threshold, value: elapsed.as_secs_f64(), threshold: threshold.as_secs_f64() }
    }
}

/// Bitrate sustained below a floor for `for_secs`. Fed one sample per tick
/// (kbps over that tick); the "sustained" timer only starts once a sample is
/// actually below the floor, so a single low tick does not itself alert.
#[derive(Debug, Clone, Copy)]
pub struct BitrateTracker {
    below_since: Option<Instant>,
}

impl BitrateTracker {
    pub fn new() -> Self {
        Self { below_since: None }
    }

    /// Drops any in-progress "below the floor" window, e.g. because the
    /// stream is gone and there is nothing left to sample.
    pub fn reset(&mut self) {
        self.below_since = None;
    }

    pub fn sample(&mut self, now: Instant, kbps: f64, floor_kbps: f64, for_secs: Duration) -> Observation {
        if kbps < floor_kbps {
            let since = *self.below_since.get_or_insert(now);
            let is_bad = now.saturating_duration_since(since) >= for_secs;
            Observation { is_bad, value: kbps, threshold: floor_kbps }
        } else {
            self.below_since = None;
            Observation { is_bad: false, value: kbps, threshold: floor_kbps }
        }
    }
}

impl Default for BitrateTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[tokio::test(start_paused = true)]
    async fn no_keyframe_fires_after_threshold_and_resolves_after_hold() {
        let start = Instant::now();
        let mut tracker = SinceTracker::seeded(start);
        let mut rule = RuleState::new();
        let threshold = secs(10);
        let min_hold = secs(5);

        // Under threshold: nothing.
        tokio::time::advance(secs(9)).await;
        let now = Instant::now();
        assert_eq!(rule.tick(now, tracker.observe(now, threshold), min_hold), None);
        assert!(!rule.is_active());

        // Crosses the threshold: fires once.
        tokio::time::advance(secs(2)).await;
        let now = Instant::now();
        let t = rule.tick(now, tracker.observe(now, threshold), min_hold);
        assert!(matches!(t, Some(Transition::Alert { .. })), "{t:?}");
        assert!(rule.is_active());

        // Still bad: no repeat alert.
        tokio::time::advance(secs(3)).await;
        let now = Instant::now();
        assert_eq!(rule.tick(now, tracker.observe(now, threshold), min_hold), None);

        // A keyframe arrives: good again, but resolved only after min_hold.
        tracker.mark(Instant::now());
        tokio::time::advance(secs(2)).await;
        let now = Instant::now();
        assert_eq!(rule.tick(now, tracker.observe(now, threshold), min_hold), None, "still within min_hold");
        assert!(rule.is_active(), "must not clear before min_hold elapses");

        // 2s (above) + 5s = min_hold fully elapsed since the first good tick.
        tokio::time::advance(secs(5)).await;
        let now = Instant::now();
        let t = rule.tick(now, tracker.observe(now, threshold), min_hold);
        assert!(matches!(t, Some(Transition::Resolved { .. })), "{t:?}");
        assert!(!rule.is_active());
    }

    #[tokio::test(start_paused = true)]
    async fn flapping_near_the_hold_boundary_never_double_fires() {
        let start = Instant::now();
        let mut tracker = SinceTracker::seeded(start);
        let mut rule = RuleState::new();
        let threshold = secs(10);
        let min_hold = secs(5);

        tokio::time::advance(secs(11)).await;
        let now = Instant::now();
        assert!(rule.tick(now, tracker.observe(now, threshold), min_hold).is_some());

        // Good, then bad again before min_hold elapses: must stay active,
        // and must not emit a second Alert (it never stopped being active).
        tracker.mark(Instant::now());
        tokio::time::advance(secs(2)).await;
        let now = Instant::now();
        assert_eq!(rule.tick(now, tracker.observe(now, threshold), min_hold), None);

        // Goes stale again before the hold elapsed: no resolved was ever
        // sent, so this must not fire a second alert either.
        tokio::time::advance(secs(11)).await;
        let now = Instant::now();
        assert_eq!(
            rule.tick(now, tracker.observe(now, threshold), min_hold),
            None,
            "no double alert while still active"
        );
        assert!(rule.is_active());
    }

    #[tokio::test(start_paused = true)]
    async fn bitrate_needs_sustained_low_samples() {
        let mut tracker = BitrateTracker::new();
        let mut rule = RuleState::new();
        let floor = 500.0;
        let for_secs = secs(10);
        let min_hold = secs(5);

        // One low sample, then recovers: never sustained, never fires.
        let mut now = Instant::now();
        assert_eq!(rule.tick(now, tracker.sample(now, 100.0, floor, for_secs), min_hold), None);
        tokio::time::advance(secs(1)).await;
        now = Instant::now();
        assert_eq!(rule.tick(now, tracker.sample(now, 900.0, floor, for_secs), min_hold), None);
        assert!(!rule.is_active());

        // Sustained low for the full window: fires.
        for _ in 0..11 {
            tokio::time::advance(secs(1)).await;
            now = Instant::now();
            let t = rule.tick(now, tracker.sample(now, 50.0, floor, for_secs), min_hold);
            if let Some(Transition::Alert { value, threshold }) = t {
                assert_eq!(value, 50.0);
                assert_eq!(threshold, floor);
            }
        }
        assert!(rule.is_active());
    }

    #[tokio::test(start_paused = true)]
    async fn since_tracker_never_seen_is_bad_immediately() {
        let tracker = SinceTracker::unset();
        let now = Instant::now();
        let obs = tracker.observe(now, secs(10));
        assert!(obs.is_bad);
    }

    #[test]
    fn rule_kind_names() {
        assert_eq!(RuleKind::NoKeyframe.as_str(), "no_keyframe");
        assert_eq!(RuleKind::MinBitrate.as_str(), "min_bitrate");
        assert_eq!(RuleKind::PublisherLost.as_str(), "publisher_lost");
        assert_eq!(RuleKind::NoAudio.as_str(), "no_audio");
    }
}
