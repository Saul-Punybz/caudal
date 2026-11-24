//! Recording schedules: `[[record.schedule]]` windows that start and stop a
//! recording by time, instead of the "always on" behaviour of `[record]
//! streams`.
//!
//! A rule names the streams it applies to (the same glob patterns as
//! `[record] streams`) and either a one-off `start` (an RFC 3339 instant) or
//! a recurring `cron` expression (5-field crontab: `min hour dom month
//! dow`, e.g. `"0 18 * * MON-FRI"`), evaluated in `tz` (an IANA zone name,
//! default UTC; ignored for `start`, which already carries its own offset).
//! `duration_secs` is how long the window stays open.
//!
//! Time handling is [`jiff`], not `chrono`/`chrono-tz`: jiff bundles the
//! IANA time zone database itself (via `jiff-tzdb`, pulled in as its default
//! feature) so `tz = "America/Puerto_Rico"` resolves the same way on every
//! machine Caudal runs on, without depending on the host's `/usr/share/
//! zoneinfo` the way `chrono-tz` still does for anything beyond the
//! database it vendors at build time; jiff's `Zoned`/`Timestamp` split also
//! makes "what wall-clock time is it in this zone right now" and "how far
//! apart are these two instants" two different, non-footgun types, which is
//! exactly the pair of operations a cron scheduler needs. Cron parsing is
//! `croner` (MIT), the actively maintained pure-Rust cron crate with a
//! native `jiff` backend (`Cron::find_next_occurrence`/
//! `find_previous_occurrence` take and return `jiff::Zoned` directly with
//! the `jiff` feature enabled), so no glue code sits between the two.
//!
//! [`Scheduler`] parses every rule once and, for each, spawns a task
//! ([`run_rule`]) that sleeps until the rule's next window, flips a
//! `watch::Sender<bool>` on, sleeps until the window ends, flips it back
//! off, and (for a `cron` rule) repeats; a `start` rule runs once. The
//! recorder merges the watches of every rule matching a stream into that
//! stream's "should I be recording right now" gate (`Scheduler::gate_for`);
//! see `recorder.rs`.
//!
//! Overlapping windows on the same rule (a `cron` firing again before
//! `duration_secs` from the previous firing has elapsed) are not merged:
//! the earlier window's stop still ends the recording, so a schedule like
//! that produces back-to-back recordings rather than one long one. Configure
//! `duration_secs` shorter than the cron's own period to avoid it.

use std::sync::Arc;
use std::time::Duration as StdDuration;

use croner::Cron;
use jiff::{Timestamp, Zoned};
use serde::Serialize;
use tokio::sync::watch;

/// `[[record.schedule]]` as parsed from TOML (`caudal::config`).
#[derive(Debug, Clone, Default)]
pub struct ScheduleConfig {
    pub streams: Vec<String>,
    /// RFC 3339 instant; mutually exclusive with `cron`.
    pub start: Option<String>,
    /// 5-field crontab (`min hour dom month dow`); mutually exclusive with `start`.
    pub cron: Option<String>,
    /// IANA zone name for `cron` (default UTC). Ignored for `start`.
    pub tz: Option<String>,
    pub duration_secs: u32,
}

/// One configured window in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub start: Timestamp,
    pub end: Timestamp,
}

#[derive(Debug, Clone)]
enum Kind {
    /// One window; exhausted once it closes. `Timestamp::MIN` /
    /// `Timestamp::MAX` stand for an open start or end.
    Once(Window),
    Cron {
        cron: Box<Cron>,
        tz: jiff::tz::TimeZone,
        duration_secs: i64,
    },
}

/// The time fields a window is written with, shared by
/// `[[record.schedule]]` and `[[push.auto]]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TimetableConfig {
    /// RFC 3339 instant the (one-off) window opens; open since forever if
    /// unset.
    pub start: Option<String>,
    /// RFC 3339 instant the (one-off) window closes; open forever if unset.
    pub stop: Option<String>,
    /// 5-field crontab: a recurring window, `duration_secs` long, opening
    /// at each occurrence. Exclusive with `start`/`stop`.
    pub cron: Option<String>,
    /// IANA zone name for `cron` (default UTC).
    pub tz: Option<String>,
    /// With `cron` (required) or `start` (instead of `stop`).
    pub duration_secs: Option<u32>,
}

/// A parsed window spec: a one-off window, or a recurring cron one. Pure:
/// it never reads the clock, every query takes `now`.
#[derive(Debug, Clone)]
pub struct Timetable {
    kind: Kind,
}

fn add_secs(ts: Timestamp, secs: i64) -> Option<Timestamp> {
    Timestamp::from_second(ts.as_second().checked_add(secs)?).ok()
}

fn instant(field: &str, s: &str) -> Result<Timestamp, String> {
    s.parse().map_err(|e| format!("bad `{field}` {s:?} (expected RFC 3339, e.g. 2026-12-24T20:00:00-04:00): {e}"))
}

impl Timetable {
    pub fn parse(cfg: &TimetableConfig) -> Result<Self, String> {
        if cfg.duration_secs == Some(0) {
            return Err("`duration_secs` must be at least 1".into());
        }
        let kind = match cfg.cron.as_deref() {
            Some(_) if cfg.start.is_some() || cfg.stop.is_some() => {
                return Err("set either `cron` or `start`/`stop`, not both".into());
            }
            Some(expr) => {
                let tz = match cfg.tz.as_deref() {
                    Some(name) => jiff::tz::TimeZone::get(name).map_err(|e| format!("bad `tz` {name:?}: {e}"))?,
                    None => jiff::tz::TimeZone::UTC,
                };
                let cron: Cron = expr.parse().map_err(|e| format!("bad `cron` {expr:?}: {e}"))?;
                let Some(d) = cfg.duration_secs else {
                    return Err("`cron` needs `duration_secs` (how long each window stays open)".into());
                };
                Kind::Cron { cron: Box::new(cron), tz, duration_secs: i64::from(d) }
            }
            None => {
                if cfg.start.is_none() && cfg.stop.is_none() {
                    return Err(if cfg.duration_secs.is_some() {
                        "`duration_secs` needs `start` or `cron`".into()
                    } else {
                        "needs `start`, `stop` or `cron`".into()
                    });
                }
                if cfg.stop.is_some() && cfg.duration_secs.is_some() {
                    return Err("set either `stop` or `duration_secs`, not both".into());
                }
                let start = cfg.start.as_deref().map(|s| instant("start", s)).transpose()?;
                let end = match (cfg.stop.as_deref(), cfg.duration_secs) {
                    (Some(s), _) => instant("stop", s)?,
                    (None, Some(d)) => match start {
                        Some(st) => add_secs(st, i64::from(d)).ok_or("`duration_secs` is out of range")?,
                        None => return Err("`duration_secs` needs `start` or `cron`".into()),
                    },
                    (None, None) => Timestamp::MAX,
                };
                let start = start.unwrap_or(Timestamp::MIN);
                if end <= start {
                    return Err("`stop` must be after `start`".into());
                }
                Kind::Once(Window { start, end })
            }
        };
        Ok(Self { kind })
    }

    /// A one-off window (it never reopens once it closes).
    pub fn is_once(&self) -> bool {
        matches!(self.kind, Kind::Once(_))
    }

    /// The window open right now, or the next one if none is. `None` once
    /// a one-off window has closed. An open-ended side is
    /// `Timestamp::MIN` / `Timestamp::MAX`.
    pub fn window_at(&self, now: Timestamp) -> Option<Window> {
        match &self.kind {
            Kind::Once(w) => (w.end > now).then_some(*w),
            Kind::Cron { cron, tz, duration_secs } => {
                let zoned_now: Zoned = now.to_zoned(tz.clone());
                if let Ok(prev) = cron.find_previous_occurrence(&zoned_now, true) {
                    let start = prev.timestamp();
                    if let Some(end) = add_secs(start, *duration_secs)
                        && end > now
                    {
                        return Some(Window { start, end });
                    }
                }
                let next = cron.find_next_occurrence(&zoned_now, false).ok()?;
                let start = next.timestamp();
                let end = add_secs(start, *duration_secs)?;
                Some(Window { start, end })
            }
        }
    }

    /// Is a window open at `now`?
    pub fn is_open(&self, now: Timestamp) -> bool {
        self.window_at(now).is_some_and(|w| w.start <= now)
    }

    /// The next instant after `now` at which [`Self::is_open`] can change:
    /// the current window's end, or the next window's start. `None` when
    /// it never changes again.
    pub fn next_change(&self, now: Timestamp) -> Option<Timestamp> {
        let w = self.window_at(now)?;
        let t = if w.start > now { w.start } else { w.end };
        (t != Timestamp::MAX).then_some(t)
    }
}

#[derive(Debug)]
struct Rule {
    streams: Vec<String>,
    timetable: Timetable,
}

impl Rule {
    fn parse(cfg: &ScheduleConfig) -> Result<Self, String> {
        if cfg.streams.is_empty() {
            return Err("needs at least one pattern in `streams`".into());
        }
        if cfg.duration_secs == 0 {
            return Err("`duration_secs` must be at least 1".into());
        }
        match (cfg.start.as_deref(), cfg.cron.as_deref()) {
            (Some(_), Some(_)) => return Err("set either `start` or `cron`, not both".into()),
            (None, None) => return Err("needs `start` or `cron`".into()),
            _ => {}
        }
        let timetable = Timetable::parse(&TimetableConfig {
            start: cfg.start.clone(),
            stop: None,
            cron: cfg.cron.clone(),
            // `tz` is documented as ignored for `start`.
            tz: if cfg.cron.is_some() { cfg.tz.clone() } else { None },
            duration_secs: Some(cfg.duration_secs),
        })?;
        Ok(Self { streams: cfg.streams.clone(), timetable })
    }

    /// The window this rule is inside of right now, or its next one if
    /// none is open. `None` only for an exhausted one-off `start`.
    fn next_after(&self, now: Timestamp) -> Option<Window> {
        self.timetable.window_at(now)
    }
}

/// Validates every entry without starting anything: pure, so `caudal check`
/// and config reload's diff can call it before a tokio runtime exists.
pub fn validate_schedules(cfgs: &[ScheduleConfig]) -> Result<(), String> {
    for (i, c) in cfgs.iter().enumerate() {
        Rule::parse(c).map_err(|e| format!("[[record.schedule]] entry {}: {e}", i + 1))?;
    }
    Ok(())
}

/// A clock abstraction so the scheduler can be tested on a paused tokio
/// clock instead of the wall clock. `RealClock::now` reads the system
/// clock; the test clock (see `tests` below) derives `now` from
/// `tokio::time::Instant`, which `tokio::time::pause`/`advance` move
/// deterministically, so `now()` and `tokio::time::sleep` stay in lockstep
/// under a paused runtime.
pub(crate) trait Clock: Send + Sync + 'static {
    fn now(&self) -> Timestamp;
}

pub(crate) struct RealClock;

impl Clock for RealClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}

struct RuleState {
    rule: Rule,
    active: watch::Sender<bool>,
}

/// Runs every parsed rule's timer task for the life of the process (they
/// are never stopped individually; `[record]` as a whole restarts on a
/// config reload, which drops this and everything it started).
pub(crate) struct Scheduler {
    rules: Vec<Arc<RuleState>>,
}

impl Scheduler {
    /// Parses `cfgs` and spawns one task per valid rule. A bad entry is
    /// logged and skipped rather than refusing every other rule — matches
    /// `[[access.rules]]`'s tolerance of a single bad entry — though by the
    /// time this runs, `to_record_config` has already called
    /// [`validate_schedules`], so this should never actually happen.
    pub fn start(cfgs: &[ScheduleConfig]) -> Self {
        Self::start_with_clock(cfgs, Arc::new(RealClock))
    }

    pub(crate) fn start_with_clock(cfgs: &[ScheduleConfig], clock: Arc<dyn Clock>) -> Self {
        let mut rules = Vec::new();
        for cfg in cfgs {
            match Rule::parse(cfg) {
                Ok(rule) => {
                    let (tx, _) = watch::channel(false);
                    let state = Arc::new(RuleState { rule, active: tx });
                    tokio::spawn(run_rule(state.clone(), clock.clone()));
                    rules.push(state);
                }
                Err(e) => tracing::error!(error = %e, "invalid [[record.schedule]] entry; skipped"),
            }
        }
        Self { rules }
    }

    /// The gate a stream not in `[record] streams` should record under:
    /// `None` if no rule's `streams` pattern matches (never scheduled),
    /// otherwise a receiver reporting whether any matching rule's window is
    /// open right now.
    pub(crate) fn gate_for(&self, name: &str) -> Option<watch::Receiver<bool>> {
        let mut matching: Vec<watch::Receiver<bool>> =
            self.rules.iter().filter(|r| crate::matches(&r.rule.streams, name)).map(|r| r.active.subscribe()).collect();
        match matching.len() {
            0 => None,
            1 => matching.pop(),
            _ => Some(merge_any(matching)),
        }
    }

    /// For `GET /api/v1/record/schedules`.
    pub(crate) fn describe(&self, now: Timestamp) -> Vec<ScheduleStatus> {
        self.rules
            .iter()
            .map(|r| {
                let window = r.rule.next_after(now);
                ScheduleStatus {
                    streams: r.rule.streams.clone(),
                    active: window.is_some_and(|w| w.start <= now),
                    next: window.map(|w| WindowInfo { start: w.start.to_string(), end: w.end.to_string() }),
                }
            })
            .collect()
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WindowInfo {
    start: String,
    end: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ScheduleStatus {
    streams: Vec<String>,
    active: bool,
    next: Option<WindowInfo>,
}

impl ScheduleStatus {
    /// For sorting the API response: the RFC 3339 start of the current-or-
    /// next window, if this rule has one.
    pub(crate) fn next_start(&self) -> Option<&str> {
        self.next.as_ref().map(|w| w.start.as_str())
    }
}

async fn run_rule(state: Arc<RuleState>, clock: Arc<dyn Clock>) {
    let once = state.rule.timetable.is_once();
    loop {
        let now = clock.now();
        let Some(win) = state.rule.next_after(now) else { return };
        if win.start > now {
            tokio::time::sleep(until_std(now, win.start)).await;
        }
        let _ = state.active.send(true);
        let now = clock.now();
        if win.end > now {
            tokio::time::sleep(until_std(now, win.end)).await;
        }
        let _ = state.active.send(false);
        if once {
            return;
        }
    }
}

fn until_std(now: Timestamp, target: Timestamp) -> StdDuration {
    StdDuration::try_from(now.duration_until(target)).unwrap_or(StdDuration::ZERO)
}

/// Merges several `active` watches into one that is true whenever any input
/// is true, without pulling in a `select_all`-shaped dependency: one task
/// per input keeps a shared count of how many are currently `true` and
/// republishes `count > 0` on every change. Only needed when more than one
/// schedule rule's `streams` pattern matches the same stream name (rare);
/// the common case (`gate_for` with exactly one match) skips this entirely.
fn merge_any(inputs: Vec<watch::Receiver<bool>>) -> watch::Receiver<bool> {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let active_count = Arc::new(AtomicUsize::new(inputs.iter().filter(|rx| *rx.borrow()).count()));
    let (tx, rx) = watch::channel(active_count.load(Ordering::Relaxed) > 0);
    for mut input in inputs {
        let active_count = active_count.clone();
        let tx = tx.clone();
        let mut last = *input.borrow();
        tokio::spawn(async move {
            while input.changed().await.is_ok() {
                let now = *input.borrow();
                if now == last {
                    continue;
                }
                last = now;
                if now {
                    active_count.fetch_add(1, Ordering::Relaxed);
                } else {
                    active_count.fetch_sub(1, Ordering::Relaxed);
                }
                if tx.send(active_count.load(Ordering::Relaxed) > 0).is_err() {
                    return;
                }
            }
        });
    }
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestClock {
        base_wall: Timestamp,
        base_instant: tokio::time::Instant,
    }

    impl TestClock {
        fn starting_at(base_wall: Timestamp) -> Arc<dyn Clock> {
            Arc::new(Self { base_wall, base_instant: tokio::time::Instant::now() })
        }
    }

    impl Clock for TestClock {
        fn now(&self) -> Timestamp {
            let elapsed = tokio::time::Instant::now().saturating_duration_since(self.base_instant);
            add_secs(self.base_wall, elapsed.as_secs() as i64).expect("in range")
        }
    }

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn once_window_before_during_after() {
        let cfg = ScheduleConfig {
            streams: vec!["cam1".into()],
            start: Some("2026-09-20T18:00:00Z".into()),
            cron: None,
            tz: None,
            duration_secs: 1800,
        };
        let rule = Rule::parse(&cfg).unwrap();
        let before = rule.next_after(ts("2026-09-20T17:00:00Z")).unwrap();
        assert_eq!(before.start, ts("2026-09-20T18:00:00Z"));
        assert_eq!(before.end, ts("2026-09-20T18:30:00Z"));

        let during = rule.next_after(ts("2026-09-20T18:15:00Z")).unwrap();
        assert_eq!(during.start, ts("2026-09-20T18:00:00Z"));

        assert!(rule.next_after(ts("2026-09-20T18:30:01Z")).is_none());
    }

    #[test]
    fn cron_window_recurs() {
        // Every day at 18:00 UTC, 60-minute window.
        let cfg = ScheduleConfig {
            streams: vec!["*".into()],
            start: None,
            cron: Some("0 18 * * *".into()),
            tz: None,
            duration_secs: 3600,
        };
        let rule = Rule::parse(&cfg).unwrap();

        let w1 = rule.next_after(ts("2026-09-20T10:00:00Z")).unwrap();
        assert_eq!(w1.start, ts("2026-09-20T18:00:00Z"));
        assert_eq!(w1.end, ts("2026-09-20T19:00:00Z"));

        // Inside the window: same window reported, marked current.
        let w2 = rule.next_after(ts("2026-09-20T18:30:00Z")).unwrap();
        assert_eq!(w2.start, w1.start);

        // After the window closes: the next day's occurrence.
        let w3 = rule.next_after(ts("2026-09-20T19:00:01Z")).unwrap();
        assert_eq!(w3.start, ts("2026-09-21T18:00:00Z"));
    }

    #[test]
    fn rejects_bad_entries() {
        let base = ScheduleConfig { streams: vec!["a".into()], start: None, cron: None, tz: None, duration_secs: 300 };
        assert!(Rule::parse(&base).unwrap_err().contains("needs `start` or `cron`"));
        assert!(
            Rule::parse(&ScheduleConfig { start: Some("x".into()), cron: Some("y".into()), ..base.clone() })
                .unwrap_err()
                .contains("not both")
        );
        assert!(
            Rule::parse(&ScheduleConfig { streams: vec![], cron: Some("* * * * *".into()), ..base.clone() })
                .unwrap_err()
                .contains("streams")
        );
        assert!(
            Rule::parse(&ScheduleConfig { duration_secs: 0, cron: Some("* * * * *".into()), ..base.clone() })
                .unwrap_err()
                .contains("duration_secs")
        );
        assert!(
            Rule::parse(&ScheduleConfig { cron: Some("not a cron".into()), ..base.clone() })
                .unwrap_err()
                .contains("bad `cron`")
        );
        assert!(
            Rule::parse(&ScheduleConfig {
                cron: Some("* * * * *".into()),
                tz: Some("Not/AZone".into()),
                ..base.clone()
            })
            .unwrap_err()
            .contains("bad `tz`")
        );
    }

    #[test]
    fn timetable_one_off_windows() {
        let tt = |start: Option<&str>, stop: Option<&str>, duration_secs: Option<u32>| {
            Timetable::parse(&TimetableConfig {
                start: start.map(Into::into),
                stop: stop.map(Into::into),
                duration_secs,
                ..Default::default()
            })
        };
        let t = tt(Some("2026-12-24T20:00:00-04:00"), Some("2026-12-24T21:00:00-04:00"), None).unwrap();
        assert!(!t.is_open(ts("2026-12-24T23:59:59Z")));
        assert_eq!(t.next_change(ts("2026-12-24T23:00:00Z")), Some(ts("2026-12-25T00:00:00Z")));
        assert!(t.is_open(ts("2026-12-25T00:00:00Z")), "open at its start");
        assert_eq!(t.next_change(ts("2026-12-25T00:30:00Z")), Some(ts("2026-12-25T01:00:00Z")));
        assert!(!t.is_open(ts("2026-12-25T01:00:00Z")), "closed at its stop");
        assert!(t.window_at(ts("2026-12-25T01:00:00Z")).is_none() && t.is_once());

        // Only `stop`: open until then. Only `start`: open forever after.
        let until = tt(None, Some("2026-12-25T00:00:00Z"), None).unwrap();
        assert!(until.is_open(ts("2000-01-01T00:00:00Z")));
        let from = tt(Some("2026-12-25T00:00:00Z"), None, None).unwrap();
        assert!(from.is_open(ts("2099-01-01T00:00:00Z")));
        assert_eq!(from.next_change(ts("2026-12-26T00:00:00Z")), None, "never closes");
        let short = tt(Some("2026-12-25T00:00:00Z"), None, Some(90)).unwrap();
        assert_eq!(short.window_at(ts("2026-12-25T00:00:00Z")).unwrap().end, ts("2026-12-25T00:01:30Z"));

        assert!(tt(Some("2026-12-25T00:00:00Z"), Some("2026-12-24T00:00:00Z"), None).unwrap_err().contains("after"));
        assert!(tt(None, None, None).unwrap_err().contains("needs"));
        assert!(tt(None, Some("2026-12-25T00:00:00Z"), Some(5)).unwrap_err().contains("not both"));
        assert!(tt(None, None, Some(5)).unwrap_err().contains("needs `start` or `cron`"));
    }

    #[tokio::test(start_paused = true)]
    async fn scheduler_flips_gate_on_and_off() {
        let clock = TestClock::starting_at(ts("2026-09-20T17:59:00Z"));
        let cfgs = vec![ScheduleConfig {
            streams: vec!["cam*".into()],
            start: None,
            cron: Some("0 18 * * *".into()),
            tz: None,
            duration_secs: 60,
        }];
        let sched = Scheduler::start_with_clock(&cfgs, clock);
        let mut gate = sched.gate_for("cam1").expect("cam1 matches cam*");
        assert!(!*gate.borrow(), "window has not opened yet");
        assert!(sched.gate_for("other").is_none(), "no rule matches `other`");

        gate.changed().await.unwrap();
        assert!(*gate.borrow(), "window opened at 18:00");

        gate.changed().await.unwrap();
        assert!(!*gate.borrow(), "window closed after 1 minute");
    }

    #[tokio::test(start_paused = true)]
    async fn merges_two_matching_rules() {
        let clock = TestClock::starting_at(ts("2026-09-20T17:59:00Z"));
        let cfgs = vec![
            ScheduleConfig {
                streams: vec!["cam1".into()],
                start: None,
                cron: Some("0 18 * * *".into()),
                tz: None,
                duration_secs: 60,
            },
            ScheduleConfig {
                streams: vec!["cam1".into()],
                start: None,
                cron: Some("5 18 * * *".into()),
                tz: None,
                duration_secs: 60,
            },
        ];
        let sched = Scheduler::start_with_clock(&cfgs, clock);
        let mut gate = sched.gate_for("cam1").expect("two rules match");
        assert!(!*gate.borrow());
        gate.changed().await.unwrap();
        assert!(*gate.borrow(), "first rule's window opened");
        gate.changed().await.unwrap();
        assert!(!*gate.borrow(), "first rule's window closed");
        gate.changed().await.unwrap();
        assert!(*gate.borrow(), "second rule's window opened");
        gate.changed().await.unwrap();
        assert!(!*gate.borrow(), "second rule's window closed");
    }
}
