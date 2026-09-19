//! Which source should be on air: a pure decision over per-source health,
//! so the rules are tested without any media.
//!
//! - Sources are ranked by configuration order; a manual switch makes its
//!   source the preferred one (rank 0) until switched back to automatic.
//! - Nothing on air: the best healthy source goes on at once.
//! - The active source is unhealthy (no frames for `switch_after`, or
//!   gone that long): the best *other* healthy source goes on at once.
//! - The active source is healthy but a better-ranked one has been
//!   healthy for `switch_back_after` without a break: switch back to it.
//!   The hold keeps a flapping primary from bouncing viewers.

use std::time::Duration;

use tokio::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// First source on air.
    Start,
    /// The active source stopped sending frames.
    Silent,
    /// A better-ranked source has been healthy for the hold time.
    Recovered,
    /// `POST /api/v1/failover/{stream}/switch`.
    Manual,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Start => "start",
            Reason::Silent => "silent",
            Reason::Recovered => "recovered",
            Reason::Manual => "manual",
        }
    }
}

pub(crate) struct Switcher {
    switch_back_after: Duration,
    healthy_since: Vec<Option<Instant>>,
    pub(crate) active: Option<usize>,
    pub(crate) preferred: Option<usize>,
}

impl Switcher {
    pub(crate) fn new(sources: usize, switch_back_after: Duration) -> Self {
        Self { switch_back_after, healthy_since: vec![None; sources], active: None, preferred: None }
    }

    fn rank(&self, i: usize) -> usize {
        if self.preferred == Some(i) { 0 } else { i + 1 }
    }

    /// Records this tick's health. Call before [`Switcher::decide`].
    pub(crate) fn observe(&mut self, now: Instant, healthy: &[bool]) {
        for (since, &ok) in self.healthy_since.iter_mut().zip(healthy) {
            match (ok, *since) {
                (true, None) => *since = Some(now),
                (false, _) => *since = None,
                _ => {}
            }
        }
    }

    /// How long source `i` has been healthy without a break.
    pub(crate) fn healthy_for(&self, now: Instant, i: usize) -> Option<Duration> {
        self.healthy_since.get(i).copied().flatten().map(|s| now.saturating_duration_since(s))
    }

    /// The switch to make now, if any. Does not apply it.
    pub(crate) fn decide(&self, now: Instant, healthy: &[bool]) -> Option<(usize, Reason)> {
        let mut by_rank: Vec<usize> = (0..healthy.len()).filter(|&i| healthy[i]).collect();
        by_rank.sort_by_key(|&i| self.rank(i));
        match self.active {
            None => by_rank.first().map(|&i| (i, Reason::Start)),
            Some(a) if !healthy.get(a).copied().unwrap_or(false) => {
                by_rank.into_iter().find(|&i| i != a).map(|i| (i, Reason::Silent))
            }
            Some(a) => by_rank
                .into_iter()
                .take_while(|&i| self.rank(i) < self.rank(a))
                .find(|&i| self.healthy_for(now, i).is_some_and(|d| d >= self.switch_back_after))
                .map(|i| (i, Reason::Recovered)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOLD: Duration = Duration::from_secs(10);

    fn step(sw: &mut Switcher, now: Instant, healthy: &[bool]) -> Option<(usize, Reason)> {
        sw.observe(now, healthy);
        let d = sw.decide(now, healthy);
        if let Some((i, _)) = d {
            sw.active = Some(i);
        }
        d
    }

    #[test]
    fn starts_on_the_best_healthy_source() {
        let t = Instant::now();
        let mut sw = Switcher::new(3, HOLD);
        assert_eq!(step(&mut sw, t, &[false, false, false]), None);
        assert_eq!(step(&mut sw, t, &[false, true, true]), Some((1, Reason::Start)));
    }

    #[test]
    fn switches_on_silence_to_the_next_healthy_source() {
        let t = Instant::now();
        let mut sw = Switcher::new(3, HOLD);
        step(&mut sw, t, &[true, true, true]);
        assert_eq!(sw.active, Some(0));
        // The backup is down too: the file (last resort) takes over.
        assert_eq!(step(&mut sw, t, &[false, false, true]), Some((2, Reason::Silent)));
        // Nothing healthy at all: stay put.
        assert_eq!(step(&mut sw, t, &[false, false, false]), None);
        assert_eq!(sw.active, Some(2));
    }

    #[test]
    fn switches_back_only_after_the_hold() {
        let t = Instant::now();
        let mut sw = Switcher::new(2, HOLD);
        step(&mut sw, t, &[true, true]);
        step(&mut sw, t, &[false, true]);
        assert_eq!(sw.active, Some(1));
        let back = t + Duration::from_secs(1);
        assert_eq!(step(&mut sw, back, &[true, true]), None);
        assert_eq!(step(&mut sw, back + Duration::from_secs(9), &[true, true]), None);
        // A blip restarts the hold.
        assert_eq!(step(&mut sw, back + Duration::from_millis(9_500), &[false, true]), None);
        let again = back + Duration::from_secs(10);
        assert_eq!(step(&mut sw, again, &[true, true]), None);
        assert_eq!(step(&mut sw, again + HOLD - Duration::from_millis(1), &[true, true]), None);
        assert_eq!(step(&mut sw, again + HOLD, &[true, true]), Some((0, Reason::Recovered)));
    }

    #[test]
    fn manual_preference_outranks_the_primary() {
        let t = Instant::now();
        let mut sw = Switcher::new(2, HOLD);
        step(&mut sw, t, &[true, true]);
        sw.preferred = Some(1);
        sw.active = Some(1);
        // The primary is healthy for ages: no switch back while pinned.
        assert_eq!(step(&mut sw, t + Duration::from_secs(60), &[true, true]), None);
        // The pinned source fails: fail over to the primary anyway.
        assert_eq!(step(&mut sw, t + Duration::from_secs(61), &[true, false]), Some((0, Reason::Silent)));
        // It is back and held: return to it, it is still preferred.
        step(&mut sw, t + Duration::from_secs(62), &[true, true]);
        assert_eq!(step(&mut sw, t + Duration::from_secs(72), &[true, true]), Some((1, Reason::Recovered)));
        // Back to automatic: the primary wins after the hold.
        sw.preferred = None;
        assert_eq!(step(&mut sw, t + Duration::from_secs(73), &[true, true]), Some((0, Reason::Recovered)));
    }
}
