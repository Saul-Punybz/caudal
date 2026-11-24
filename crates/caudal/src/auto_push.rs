//! Automatic pushes (`[[push.auto]]`, MistServer's "auto push"): a rule
//! pairs stream patterns with a target URL template; whenever a matching
//! stream goes live, the push starts by itself, and when the stream ends
//! it stops.
//!
//! No new sender: a live match becomes one more target of the existing
//! engine for its scheme (`caudal_restream`, `caudal_srt`'s push,
//! `caudal_multicast`'s UDP/RTP sender, `caudal_webrtc`'s WHIP push), next
//! to the static `[[restream]]`, `[[srt.push]]`, `[[multicast]]` /
//! `[[udp.push]]`, `[[webrtc.push]]` entries. [`AutoPush`] owns those four
//! target lists: on every publish
//! or end, and on a config reload, it recomputes `static ∪ expansions` and
//! hands each engine its list. The engines' `reload` diffs by identity, so
//! a target that did not change keeps its task and connection: an
//! unrelated push is never restarted.
//!
//! Gates: a rule may carry a time window (`start`/`stop`, or `cron` +
//! `duration_secs`, parsed by `caudal_record::Timetable`, the same code
//! as `[[record.schedule]]`), an `enabled` flag (overridable at runtime
//! from the API) and `inhibit` patterns (no push while a matching stream
//! is live). A closed gate makes the rule push nothing; the recompute is
//! the same one, so opening or closing a window touches only that rule's
//! targets. A timer task recomputes at the next window boundary of any
//! rule (and at least once a minute, so a wall-clock jump is caught).
//!
//! Dedupe: a static entry and an expansion that produce the same target
//! (same stream and URL) run once, as the static one; two rules that expand
//! to the same target also run once. For UDP/RTP the key is the destination
//! address (one stream per destination, as for the static entries): config
//! check refuses a rule whose destination a static entry or another rule
//! already uses, and while several streams match one UDP rule, the stream
//! that already holds the destination keeps it (otherwise the first by
//! name) and the others are reported as skipped.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use caudal_core::Registry;
use caudal_record::Timetable;
use jiff::Timestamp;
use parking_lot::Mutex;
use serde::Serialize;
use tokio::sync::Notify;
use tokio::sync::broadcast::error::RecvError;

use crate::config::{AutoPushEntry, AutoPushKind, Config, expand_push_template};

/// Wall-clock time; injectable so tests run windows on tokio's paused clock.
pub type Clock = Arc<dyn Fn() -> Timestamp + Send + Sync>;

/// The longest the timer sleeps between two recomputes, so a wall-clock
/// jump (NTP step, suspend) is noticed within this long.
const MAX_TIMER_SLEEP: Duration = Duration::from_secs(60);

/// The statically configured targets of the three push engines, and the
/// auto rules on top of them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PushSet {
    pub restream: Vec<caudal_restream::RestreamTarget>,
    pub srt: Vec<caudal_srt::SrtPush>,
    pub udp: Vec<caudal_multicast::MulticastTarget>,
    pub whip: Vec<caudal_webrtc::WhipPush>,
    pub rules: Vec<AutoPushEntry>,
}

impl PushSet {
    /// From a validated config.
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            restream: cfg.restream_targets(),
            srt: cfg.srt.push_targets(),
            udp: cfg.multicast_targets().expect("validated"),
            whip: cfg.webrtc.push_targets(),
            rules: cfg.push.auto.clone(),
        }
    }
}

/// One rule as `GET /api/v1/auto-pushes` shows it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RuleJson {
    /// Position in `[[push.auto]]`, for `POST /api/v1/auto-pushes/{index}/...`.
    pub index: usize,
    pub streams: Vec<String>,
    /// The template, secrets masked like every push listing.
    pub url: String,
    /// `rtmp`, `srt`, `udp` or `whip`: the engine the pushes run on.
    pub kind: &'static str,
    /// The file's `enabled`, or the API's override of it.
    pub enabled: bool,
    /// The time window; `null` for a rule without one (always on).
    pub schedule: Option<ScheduleJson>,
    pub inhibit: Vec<String>,
    /// Why the rule pushes nothing right now; `null` while it pushes.
    pub idle: Option<IdleJson>,
    /// The pushes this rule runs now, one per live matching stream.
    pub active: Vec<ActiveJson>,
    /// Matching live streams this rule does not push, and why.
    pub skipped: Vec<SkippedJson>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ScheduleJson {
    /// Is the window open now?
    pub open: bool,
    /// The window open now, or the next one; `null` when none is left (a
    /// one-off window that has closed).
    pub window: Option<WindowJson>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WindowJson {
    /// RFC 3339, UTC; `null` for a window open since forever.
    pub start: Option<String>,
    /// RFC 3339, UTC; `null` for a window that never closes.
    pub stop: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct IdleJson {
    /// `disabled`, `outside_window`, `inhibited` or `offline` (no matching
    /// stream is live), checked in that order.
    pub reason: &'static str,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ActiveJson {
    pub stream: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SkippedJson {
    pub stream: String,
    pub reason: String,
}

/// What one recompute decided: the engines' full target lists, and the
/// per-rule view for the API.
#[derive(Debug, Default, PartialEq)]
struct Plan {
    restream: Vec<caudal_restream::RestreamTarget>,
    srt: Vec<caudal_srt::SrtPush>,
    udp: Vec<caudal_multicast::MulticastTarget>,
    whip: Vec<caudal_webrtc::WhipPush>,
    rules: Vec<RuleJson>,
}

/// A rule's gates other than its streams, resolved for one recompute.
struct Gates<'a> {
    enabled: bool,
    timetable: Option<&'a Timetable>,
}

fn rfc3339(t: Timestamp) -> Option<String> {
    (t != Timestamp::MIN && t != Timestamp::MAX).then(|| t.to_string())
}

/// Why `rule` (with `gates`) pushes nothing at `now`, if it doesn't.
fn idle_reason(
    rule: &AutoPushEntry,
    gates: &Gates,
    schedule: Option<&ScheduleJson>,
    live: &[String],
    any_match: bool,
) -> Option<IdleJson> {
    if !gates.enabled {
        return Some(IdleJson { reason: "disabled", detail: "the rule is disabled".into() });
    }
    if let Some(s) = schedule.filter(|s| !s.open) {
        let detail = match s.window.as_ref().and_then(|w| w.start.as_deref()) {
            Some(start) => format!("outside its schedule; the next window opens at {start}"),
            None => "outside its schedule; no window is left".into(),
        };
        return Some(IdleJson { reason: "outside_window", detail });
    }
    if let Some(by) = rule.inhibited_by(live) {
        return Some(IdleJson { reason: "inhibited", detail: format!("inhibited while stream {by:?} is live") });
    }
    (!any_match).then(|| IdleJson { reason: "offline", detail: "no matching stream is live".into() })
}

/// `set`'s static targets plus every rule expanded for each name in `live`
/// (sorted). `held` maps a UDP destination to the stream an auto push
/// already sends there, so a running one is never displaced. `gates` is
/// one entry per rule.
fn plan(
    set: &PushSet,
    live: &[String],
    held: &BTreeMap<std::net::SocketAddr, String>,
    gates: &[Gates],
    now: Timestamp,
) -> (Plan, BTreeMap<std::net::SocketAddr, String>) {
    let mut p = Plan {
        restream: set.restream.clone(),
        srt: set.srt.clone(),
        udp: set.udp.clone(),
        whip: set.whip.clone(),
        rules: vec![],
    };
    let mut now_held = BTreeMap::new();
    for (index, (rule, gates)) in set.rules.iter().zip(gates).enumerate() {
        let kind = rule.kind().expect("validated");
        let schedule = gates.timetable.map(|t| ScheduleJson {
            open: t.is_open(now),
            window: t.window_at(now).map(|w| WindowJson { start: rfc3339(w.start), stop: rfc3339(w.end) }),
        });
        let matching: Vec<&String> = live.iter().filter(|n| rule.matches(n)).collect();
        let idle = idle_reason(rule, gates, schedule.as_ref(), live, !matching.is_empty());
        let mut json = RuleJson {
            index,
            streams: rule.streams.clone(),
            url: crate::config_edit::redact(&rule.url),
            kind: kind.as_str(),
            enabled: gates.enabled,
            schedule,
            inhibit: rule.inhibit.clone(),
            idle,
            active: vec![],
            skipped: vec![],
        };
        if json.idle.is_some() {
            p.rules.push(json);
            continue;
        }
        match kind {
            AutoPushKind::Restream => {
                for name in matching {
                    let url = expand_push_template(&rule.url, name);
                    let t = caudal_restream::RestreamTarget { stream: name.clone(), url };
                    json.active.push(ActiveJson { stream: name.clone(), url: crate::config_edit::redact(&t.url) });
                    if !p.restream.contains(&t) {
                        p.restream.push(t);
                    }
                }
            }
            AutoPushKind::Srt => {
                for name in matching {
                    let url = expand_push_template(&rule.url, name);
                    let t = caudal_srt::SrtPush { stream: name.clone(), url };
                    json.active.push(ActiveJson { stream: name.clone(), url: crate::config_edit::redact(&t.url) });
                    if !p.srt.contains(&t) {
                        p.srt.push(t);
                    }
                }
            }
            AutoPushKind::Whip => {
                for name in matching {
                    let url = expand_push_template(&rule.url, name);
                    let t = caudal_webrtc::WhipPush { stream: name.clone(), url, token: rule.token.clone() };
                    json.active.push(ActiveJson { stream: name.clone(), url: crate::config_edit::redact(&t.url) });
                    if !p.whip.iter().any(|w| w.stream == t.stream && w.url == t.url) {
                        p.whip.push(t);
                    }
                }
            }
            AutoPushKind::Udp => {
                // Not idle, so at least one stream matches. Config check
                // guarantees a fixed destination per rule, not shared with
                // a static entry or another rule.
                let first = matching[0];
                let group = rule.udp_target(first).expect("validated").group;
                let owner = held
                    .get(&group)
                    .filter(|s| matching.iter().any(|m| m == s))
                    .cloned()
                    .unwrap_or_else(|| first.clone());
                for name in matching {
                    if *name == owner {
                        let t = rule.udp_target(name).expect("validated");
                        json.active.push(ActiveJson { stream: name.clone(), url: rule.url.clone() });
                        now_held.insert(group, name.clone());
                        p.udp.push(t);
                    } else {
                        json.skipped.push(SkippedJson {
                            stream: name.clone(),
                            reason: format!("destination {group} is taken by stream {owner:?}"),
                        });
                    }
                }
            }
        }
        p.rules.push(json);
    }
    (p, now_held)
}

struct State {
    set: PushSet,
    /// One per rule in `set.rules`, parsed once per `configure`.
    timetables: Vec<Option<Timetable>>,
    /// `enabled` set over the API, keyed by the whole rule: a rule edited
    /// in the file (even just its `enabled`) loses its override, an
    /// unrelated reload keeps it.
    overrides: Vec<(AutoPushEntry, bool)>,
    /// What each engine was last given, so an event that changes nothing
    /// for an engine doesn't call its `reload`.
    applied: Plan,
    held: BTreeMap<std::net::SocketAddr, String>,
    /// Skips already logged, so a warning is printed once per stream.
    warned: HashSet<(usize, String)>,
}

fn timetables(set: &PushSet) -> Vec<Option<Timetable>> {
    set.rules.iter().map(|r| r.timetable().expect("validated")).collect()
}

/// The auto-push rules and the four engines they feed. Cheap to clone.
#[derive(Clone)]
pub struct AutoPush {
    inner: Arc<Inner>,
}

struct Inner {
    registry: Arc<Registry>,
    restream: caudal_restream::RestreamHandle,
    srt: caudal_srt::PushHandle,
    udp: caudal_multicast::MulticastHandle,
    whip: caudal_webrtc::WhipPushHandle,
    clock: Clock,
    /// Wakes the timer task after a change that may move the next window
    /// boundary (a reload).
    timer: Arc<Notify>,
    state: Mutex<State>,
}

impl AutoPush {
    /// Takes over the target lists of the four engines (started with
    /// `set`'s static targets) and starts watching publishes and ends.
    /// Must be called inside a tokio runtime.
    pub fn start(
        registry: Arc<Registry>,
        set: PushSet,
        restream: caudal_restream::RestreamHandle,
        srt: caudal_srt::PushHandle,
        udp: caudal_multicast::MulticastHandle,
        whip: caudal_webrtc::WhipPushHandle,
    ) -> Self {
        Self::start_with_clock(registry, set, restream, srt, udp, whip, Arc::new(Timestamp::now))
    }

    pub(crate) fn start_with_clock(
        registry: Arc<Registry>,
        set: PushSet,
        restream: caudal_restream::RestreamHandle,
        srt: caudal_srt::PushHandle,
        udp: caudal_multicast::MulticastHandle,
        whip: caudal_webrtc::WhipPushHandle,
        clock: Clock,
    ) -> Self {
        let applied = Plan {
            restream: set.restream.clone(),
            srt: set.srt.clone(),
            udp: set.udp.clone(),
            whip: set.whip.clone(),
            rules: vec![],
        };
        let state = State {
            timetables: timetables(&set),
            set,
            overrides: vec![],
            applied,
            held: BTreeMap::new(),
            warned: HashSet::new(),
        };
        let inner = Arc::new(Inner {
            registry: registry.clone(),
            restream,
            srt,
            udp,
            whip,
            clock,
            timer: Arc::new(Notify::new()),
            state: Mutex::new(state),
        });
        // Subscribe before the first recompute, so a stream published in
        // between is seen (a recompute is idempotent).
        let mut publishes = registry.subscribe_publishes();
        let mut ends = registry.subscribe_ends();
        inner.recompute();
        // Weak: dropping every `AutoPush` ends the watcher.
        let weak = Arc::downgrade(&inner);
        tokio::spawn(async move {
            loop {
                let closed = tokio::select! {
                    r = publishes.recv() => matches!(r, Err(RecvError::Closed)),
                    r = ends.recv() => matches!(r, Err(RecvError::Closed)),
                };
                let Some(inner) = weak.upgrade() else { break };
                if closed {
                    break;
                }
                // Lagged or not, the registry is the truth: recompute.
                inner.recompute();
            }
        });
        // The schedule timer: sleeps until the next window boundary of any
        // rule (at most `MAX_TIMER_SLEEP`), or until a reload, and
        // recomputes. Ends with the last `AutoPush`, within a sleep.
        let weak = Arc::downgrade(&inner);
        tokio::spawn(async move {
            loop {
                let Some(inner) = weak.upgrade() else { break };
                let wait = inner.until_next_change();
                let timer = inner.timer.clone();
                drop(inner);
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = timer.notified() => {}
                }
                let Some(inner) = weak.upgrade() else { break };
                inner.recompute();
            }
        });
        Self { inner }
    }

    /// Applies a new configuration (static targets and rules); a target
    /// that is in both the old and the new result keeps running.
    pub fn configure(&self, set: PushSet) {
        {
            let mut st = self.inner.state.lock();
            st.timetables = timetables(&set);
            st.overrides.retain(|(r, _)| set.rules.contains(r));
            st.set = set;
        }
        self.inner.recompute();
        self.inner.timer.notify_one();
    }

    /// Overrides rule `index`'s `enabled` until the rule changes in the
    /// file or the process restarts. `false` when there is no such rule.
    pub fn set_enabled(&self, index: usize, enabled: bool) -> bool {
        {
            let mut st = self.inner.state.lock();
            let Some(rule) = st.set.rules.get(index).cloned() else { return false };
            st.overrides.retain(|(r, _)| *r != rule);
            st.overrides.push((rule, enabled));
        }
        self.inner.recompute();
        true
    }

    /// Every rule with what it pushes right now.
    pub fn rules(&self) -> Vec<RuleJson> {
        self.inner.state.lock().applied.rules.clone()
    }
}

impl Inner {
    /// How long the timer may sleep before some rule's window opens or
    /// closes.
    fn until_next_change(&self) -> Duration {
        let now = (self.clock)();
        let st = self.state.lock();
        st.timetables.iter().flatten().filter_map(|t| t.next_change(now)).min().map_or(MAX_TIMER_SLEEP, |t| {
            Duration::try_from(now.duration_until(t)).unwrap_or(Duration::ZERO).min(MAX_TIMER_SLEEP)
        })
    }

    fn recompute(&self) {
        // Held for the whole apply, and taken before reading the registry:
        // a publish event and a config reload never interleave, and the
        // last one to apply always saw the newest stream list.
        let mut st = self.state.lock();
        let now = (self.clock)();
        let live: Vec<String> =
            self.registry.list().into_iter().filter(|s| !s.is_ended()).map(|s| s.name().to_owned()).collect();
        let gates: Vec<Gates> = st
            .set
            .rules
            .iter()
            .zip(&st.timetables)
            .map(|(rule, timetable)| Gates {
                enabled: st.overrides.iter().find(|(r, _)| r == rule).map_or(rule.enabled, |(_, on)| *on),
                timetable: timetable.as_ref(),
            })
            .collect();
        let (next, held) = plan(&st.set, &live, &st.held, &gates, now);
        drop(gates);
        let mut warned = HashSet::new();
        for (i, rule) in next.rules.iter().enumerate() {
            for s in &rule.skipped {
                let key = (i, s.stream.clone());
                if !st.warned.contains(&key) {
                    tracing::warn!(stream = %s.stream, rule = %rule.url, reason = %s.reason, "auto push skipped");
                }
                warned.insert(key);
            }
        }
        st.warned = warned;
        for (before, after) in st.applied.rules.iter().zip(&next.rules) {
            let reason = |r: &RuleJson| r.idle.as_ref().map(|i| i.reason);
            if reason(before) != reason(after)
                && let Some(idle) = &after.idle
                && idle.reason != "offline"
            {
                tracing::info!(rule = after.index, url = %after.url, reason = %idle.detail, "auto push rule idle");
            }
        }
        let active = |p: &Plan| -> HashSet<(String, String)> {
            p.rules.iter().flat_map(|r| r.active.iter().map(|a| (a.stream.clone(), a.url.clone()))).collect()
        };
        let (before, after) = (active(&st.applied), active(&next));
        for (stream, target) in after.difference(&before) {
            tracing::info!(%stream, %target, "auto push started");
        }
        for (stream, target) in before.difference(&after) {
            tracing::info!(%stream, %target, "auto push stopped");
        }
        if next.restream != st.applied.restream {
            self.restream.reload(&self.registry, next.restream.clone());
        }
        if next.srt != st.applied.srt {
            self.srt.reload(&self.registry, next.srt.clone());
        }
        if next.udp != st.applied.udp {
            self.udp.reload(&self.registry, next.udp.clone());
        }
        if next.whip != st.applied.whip {
            self.whip.reload(&self.registry, next.whip.clone());
        }
        st.held = held;
        st.applied = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(streams: &[&str], url: &str) -> AutoPushEntry {
        AutoPushEntry {
            streams: streams.iter().map(|s| s.to_string()).collect(),
            url: url.into(),
            ttl: None,
            interface: None,
            pacing: None,
            token: None,
            start: None,
            stop: None,
            cron: None,
            tz: None,
            duration_secs: None,
            enabled: true,
            inhibit: vec![],
        }
    }

    fn always(set: &PushSet) -> Vec<Gates<'static>> {
        set.rules.iter().map(|r| Gates { enabled: r.enabled, timetable: None }).collect()
    }

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    /// A clock that starts at `wall` and moves with tokio's (paused) clock.
    fn test_clock(wall: &str) -> Clock {
        let wall = ts(wall);
        let base = tokio::time::Instant::now();
        Arc::new(move || wall + jiff::SignedDuration::try_from(base.elapsed()).unwrap())
    }

    fn engines(
        registry: &Arc<Registry>,
    ) -> (caudal_restream::RestreamHandle, caudal_srt::PushHandle, caudal_multicast::MulticastHandle) {
        (
            caudal_restream::start(registry.clone(), caudal_restream::RestreamConfig { targets: vec![] }),
            caudal_srt::start_pushes(registry.clone(), vec![]),
            caudal_multicast::start(registry.clone(), vec![]),
        )
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn expands_per_matching_stream_and_dedupes_with_static() {
        let set = PushSet {
            srt: vec![caudal_srt::SrtPush { stream: "cam1".into(), url: "srt://h:9000?streamid=cam1".into() }],
            rules: vec![
                rule(&["cam*"], "srt://h:9000?streamid=$stream"),
                rule(&["main"], "rtmp://yt.example/live/${stream}-key"),
                // Same target as the first rule for cam2: runs once.
                rule(&["cam2"], "srt://h:9000?streamid=cam2"),
            ],
            ..Default::default()
        };
        let (p, _) = plan(
            &set,
            &names(&["cam1", "cam2", "cam2+720p", "main", "other"]),
            &BTreeMap::new(),
            &always(&set),
            Timestamp::now(),
        );
        let srt: Vec<_> = p.srt.iter().map(|t| (t.stream.as_str(), t.url.as_str())).collect();
        assert_eq!(srt, [("cam1", "srt://h:9000?streamid=cam1"), ("cam2", "srt://h:9000?streamid=cam2")]);
        assert_eq!(p.restream.len(), 1);
        assert_eq!(p.restream[0].url, "rtmp://yt.example/live/main-key");
        assert_eq!(p.rules[0].active.len(), 2, "cam2+720p never matches");
        assert_eq!(p.rules[1].active[0].url, "rtmp://yt.example/live/****", "keys are masked");
        assert!(p.udp.is_empty());
    }

    #[test]
    fn whip_rules_expand_with_their_token() {
        let set = PushSet {
            whip: vec![caudal_webrtc::WhipPush {
                stream: "cam1".into(),
                url: "whip://h/whip/cam1".into(),
                token: None,
            }],
            rules: vec![AutoPushEntry { token: Some("tk".into()), ..rule(&["cam*"], "whip://h/whip/$stream") }],
            ..Default::default()
        };
        let (p, _) = plan(&set, &names(&["cam1", "cam2", "other"]), &BTreeMap::new(), &always(&set), Timestamp::now());
        let got: Vec<_> = p.whip.iter().map(|t| (t.stream.as_str(), t.url.as_str(), t.token.as_deref())).collect();
        // cam1 is the static entry already (runs once, as the static one).
        assert_eq!(got, [("cam1", "whip://h/whip/cam1", None), ("cam2", "whip://h/whip/cam2", Some("tk"))]);
        assert_eq!(p.rules[0].kind, "whip");
        assert_eq!(p.rules[0].active.len(), 2);
    }

    #[test]
    fn udp_destination_stays_with_its_stream() {
        let set = PushSet { rules: vec![rule(&["*"], "udp://127.0.0.1:5000")], ..Default::default() };
        let g = always(&set);
        let now = Timestamp::now();
        let (p, held) = plan(&set, &names(&["a", "b"]), &BTreeMap::new(), &g, now);
        assert_eq!(p.udp.len(), 1);
        assert_eq!(p.udp[0].stream, "a");
        assert_eq!(p.rules[0].skipped[0].stream, "b");
        // `a0` sorts first but `a` holds the destination.
        let (p, _) = plan(&set, &names(&["a", "a0", "b"]), &held, &g, now);
        assert_eq!(p.udp[0].stream, "a");
        // Once `a` ends, the next one takes it.
        let (p, _) = plan(&set, &names(&["a0", "b"]), &held, &g, now);
        assert_eq!(p.udp[0].stream, "a0");
    }

    #[tokio::test]
    async fn configure_keeps_unrelated_targets() {
        let registry = Registry::new();
        let set = PushSet {
            restream: vec![caudal_restream::RestreamTarget { stream: "s".into(), url: "rtmp://x/a/k".into() }],
            ..Default::default()
        };
        let restream =
            caudal_restream::start(registry.clone(), caudal_restream::RestreamConfig { targets: set.restream.clone() });
        let srt = caudal_srt::start_pushes(registry.clone(), vec![]);
        let udp = caudal_multicast::start(registry.clone(), vec![]);
        let whip = caudal_webrtc::start_pushes(registry.clone(), vec![]);
        let auto = AutoPush::start(registry.clone(), set.clone(), restream, srt, udp, whip);
        let _p = registry.publish("cam", caudal_core::BufferConfig::default()).unwrap();
        let mut with_rule = set.clone();
        with_rule.rules.push(rule(&["cam"], "rtmp://y/live/$stream"));
        auto.configure(with_rule);
        let rules = auto.rules();
        assert_eq!(rules[0].active, [ActiveJson { stream: "cam".into(), url: "rtmp://y/live/****".into() }]);
        let applied = auto.inner.state.lock().applied.restream.clone();
        assert_eq!(applied.len(), 2);
        assert_eq!(applied[0].stream, "s", "the static target stays first and untouched");
        auto.configure(set);
        assert_eq!(auto.inner.state.lock().applied.restream.len(), 1);
    }

    fn aac() -> caudal_core::TrackInfo {
        let asc: u16 = (2u16 << 11) | (3u16 << 7) | (2u16 << 3);
        caudal_core::TrackInfo {
            id: caudal_core::TrackId(0),
            codec: caudal_core::Codec::Aac,
            timescale: 48_000,
            init: bytes::Bytes::copy_from_slice(&asc.to_be_bytes()),
            lang: None,
            video: None,
            audio: Some(caudal_core::AudioParams { sample_rate: 48_000, channels: 2 }),
        }
    }

    /// Publishes `name` with one second of AAC frames; returns the publisher.
    fn publish(registry: &Arc<Registry>, name: &str) -> caudal_core::Publisher {
        let p = registry.publish(name, caudal_core::BufferConfig::default()).unwrap();
        p.set_tracks(vec![aac()]).unwrap();
        p
    }

    fn feed(p: &caudal_core::Publisher, from: i64, n: i64) {
        for i in from..from + n {
            let dts = i * 1024;
            p.push(caudal_core::Frame {
                track: caudal_core::TrackId(0),
                dts,
                pts: dts,
                keyframe: true,
                data: bytes::Bytes::from(vec![0x21; 200]),
            })
            .unwrap();
        }
    }

    /// Datagrams that arrive on `sock` within `wait`.
    async fn received(sock: &tokio::net::UdpSocket, wait: std::time::Duration) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = [0u8; 2048];
        let deadline = tokio::time::Instant::now() + wait;
        while let Ok(Ok(n)) = tokio::time::timeout_at(deadline, sock.recv(&mut buf)).await {
            out.push(buf[..n].to_vec());
        }
        out
    }

    async fn until(what: &str, mut f: impl FnMut() -> bool) {
        let t0 = std::time::Instant::now();
        while !f() {
            assert!(t0.elapsed() < std::time::Duration::from_secs(5), "timed out waiting for {what}");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// A UDP rule really delivers MPEG-TS to a local socket for a matching
    /// stream, never for a non-matching one, and stops when the stream ends.
    #[tokio::test]
    async fn udp_rule_delivers_ts_while_matching_stream_is_live() {
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = sock.local_addr().unwrap().port();
        let registry = Registry::new();
        let set = PushSet {
            rules: vec![AutoPushEntry { pacing: Some(false), ..rule(&["cam*"], &format!("udp://127.0.0.1:{port}")) }],
            ..Default::default()
        };
        let restream = caudal_restream::start(registry.clone(), caudal_restream::RestreamConfig { targets: vec![] });
        let srt = caudal_srt::start_pushes(registry.clone(), vec![]);
        let udp = caudal_multicast::start(registry.clone(), vec![]);
        let whip = caudal_webrtc::start_pushes(registry.clone(), vec![]);
        let auto = AutoPush::start(registry.clone(), set, restream, srt, udp, whip);

        // Not matching: nothing is sent.
        let other = publish(&registry, "studio");
        feed(&other, 0, 100);
        assert!(received(&sock, Duration::from_millis(500)).await.is_empty(), "no push for a non-matching stream");
        assert!(auto.rules()[0].active.is_empty());

        // Matching: TS arrives, seven 188-byte packets per datagram.
        let cam = publish(&registry, "cam1");
        until("the auto push to start", || !auto.rules()[0].active.is_empty()).await;
        let mut got = Vec::new();
        for round in 0..20 {
            feed(&cam, round * 50, 50);
            got.extend(received(&sock, Duration::from_millis(100)).await);
            if !got.is_empty() {
                break;
            }
        }
        assert!(!got.is_empty(), "the auto push delivered nothing");
        for d in &got {
            assert_eq!(d.len() % 188, 0, "whole TS packets");
            assert!(d.chunks(188).all(|p| p[0] == 0x47), "TS sync bytes");
        }

        // Ending the stream stops the push: the target goes away.
        drop(cam);
        until("the auto push to stop", || auto.rules()[0].active.is_empty()).await;
        assert!(auto.inner.state.lock().applied.udp.is_empty());
        received(&sock, Duration::from_millis(200)).await; // drain
        feed(&other, 100, 100);
        assert!(received(&sock, Duration::from_millis(300)).await.is_empty(), "nothing after the end");
    }

    #[test]
    fn gates_make_a_rule_idle_and_say_why() {
        let tt = |start: &str, stop: &str| {
            Timetable::parse(&caudal_record::TimetableConfig {
                start: Some(start.into()),
                stop: Some(stop.into()),
                ..Default::default()
            })
            .unwrap()
        };
        let window = tt("2026-12-24T20:00:00-04:00", "2026-12-24T23:00:00-04:00");
        let set = PushSet {
            rules: vec![
                rule(&["cam*"], "srt://h:9000?streamid=$stream"),
                AutoPushEntry { inhibit: vec!["main".into()], ..rule(&["backup"], "rtmp://b/live/$stream") },
                rule(&["show"], "rtmp://c/live/$stream"),
                rule(&["nobody"], "rtmp://d/live/$stream"),
            ],
            ..Default::default()
        };
        let gates = |enabled0: bool| {
            vec![
                Gates { enabled: enabled0, timetable: None },
                Gates { enabled: true, timetable: None },
                Gates { enabled: true, timetable: Some(&window) },
                Gates { enabled: true, timetable: None },
            ]
        };
        let live = names(&["backup", "cam1", "main", "show"]);
        let before = ts("2026-12-24T23:59:00Z"); // 19:59 at -04:00
        let (p, _) = plan(&set, &live, &BTreeMap::new(), &gates(false), before);
        let reasons: Vec<_> = p.rules.iter().map(|r| r.idle.as_ref().map(|i| i.reason)).collect();
        assert_eq!(reasons, [Some("disabled"), Some("inhibited"), Some("outside_window"), Some("offline")]);
        assert!(p.srt.is_empty() && p.restream.is_empty(), "an idle rule adds no target");
        assert_eq!(p.rules[1].idle.as_ref().unwrap().detail, "inhibited while stream \"main\" is live");
        assert_eq!(
            p.rules[2].schedule,
            Some(ScheduleJson {
                open: false,
                window: Some(WindowJson {
                    start: Some("2026-12-25T00:00:00Z".into()),
                    stop: Some("2026-12-25T03:00:00Z".into())
                })
            })
        );
        assert!(p.rules[2].idle.as_ref().unwrap().detail.contains("2026-12-25T00:00:00Z"));

        // Enabled, `main` gone, window open: rules 0-2 push.
        let live = names(&["backup", "cam1", "show"]);
        let (p, _) = plan(&set, &live, &BTreeMap::new(), &gates(true), ts("2026-12-25T00:00:00Z"));
        assert!(p.rules[..3].iter().all(|r| r.idle.is_none() && r.active.len() == 1), "{:?}", p.rules);
        assert_eq!(p.restream.len(), 2);
        assert_eq!(p.srt.len(), 1);
        assert!(p.rules[2].schedule.as_ref().unwrap().open);

        // After the window: closed, and no window left.
        let (p, _) = plan(&set, &live, &BTreeMap::new(), &gates(true), ts("2026-12-25T03:00:00Z"));
        let s = p.rules[2].schedule.as_ref().unwrap();
        assert!(!s.open && s.window.is_none());
        assert_eq!(p.rules[2].idle.as_ref().unwrap().detail, "outside its schedule; no window is left");
    }

    /// `set_enabled` overrides the file's flag; a reload that keeps the
    /// rule keeps the override, one that edits the rule drops it.
    #[tokio::test]
    async fn api_override_of_enabled() {
        let registry = Registry::new();
        let (restream, srt, udp) = engines(&registry);
        let r = rule(&["cam"], "rtmp://y/live/$stream");
        let set = PushSet { rules: vec![r.clone()], ..Default::default() };
        let whip = caudal_webrtc::start_pushes(registry.clone(), vec![]);
        let auto = AutoPush::start(registry.clone(), set.clone(), restream, srt, udp, whip);
        let _p = registry.publish("cam", caudal_core::BufferConfig::default()).unwrap();
        until("the push to start", || auto.rules()[0].active.len() == 1).await;
        assert!(auto.set_enabled(0, false));
        assert!(!auto.set_enabled(1, false), "no such rule");
        let rules = auto.rules();
        assert_eq!((rules[0].enabled, rules[0].idle.as_ref().unwrap().reason), (false, "disabled"));
        assert!(auto.inner.state.lock().applied.restream.is_empty(), "the push stopped");
        // An unrelated change keeps the override.
        let mut more = set.clone();
        more.restream.push(caudal_restream::RestreamTarget { stream: "s".into(), url: "rtmp://x/a/k".into() });
        auto.configure(more);
        assert!(!auto.rules()[0].enabled);
        // Editing the rule in the file drops it.
        let edited = PushSet { rules: vec![AutoPushEntry { url: "rtmp://z/live/$stream".into(), ..r }], ..set };
        auto.configure(edited);
        assert!(auto.rules()[0].enabled && auto.rules()[0].active.len() == 1);
    }

    /// On tokio's paused clock: a UDP rule with a one-off window pushes
    /// nothing before it opens, delivers TS while it is open, and stops when
    /// it closes, with no real waiting.
    #[tokio::test(start_paused = true)]
    async fn udp_rule_follows_its_schedule_window() {
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = sock.local_addr().unwrap().port();
        let registry = Registry::new();
        let set = PushSet {
            rules: vec![AutoPushEntry {
                pacing: Some(false),
                start: Some("2026-12-24T20:00:00-04:00".into()),
                stop: Some("2026-12-24T21:00:00-04:00".into()),
                ..rule(&["cam*"], &format!("udp://127.0.0.1:{port}"))
            }],
            ..Default::default()
        };
        let (restream, srt, udp) = engines(&registry);
        // Ten minutes before the window.
        let clock = test_clock("2026-12-24T19:50:00-04:00");
        let whip = caudal_webrtc::start_pushes(registry.clone(), vec![]);
        let auto = AutoPush::start_with_clock(registry.clone(), set, restream, srt, udp, whip, clock);
        let cam = publish(&registry, "cam1");
        until("the publish to be seen", || auto.rules()[0].idle.as_ref().is_some_and(|i| i.reason != "offline")).await;
        assert_eq!(auto.rules()[0].idle.as_ref().unwrap().reason, "outside_window");
        feed(&cam, 0, 100);
        assert!(received(&sock, Duration::from_millis(200)).await.is_empty(), "nothing before the window");

        // The window opens: the timer starts the push.
        tokio::time::advance(Duration::from_secs(600)).await;
        until("the window to open", || !auto.rules()[0].active.is_empty()).await;
        assert!(auto.rules()[0].schedule.as_ref().unwrap().open);
        let mut got = Vec::new();
        for round in 0..20 {
            feed(&cam, 100 + round * 50, 50);
            got.extend(received(&sock, Duration::from_millis(100)).await);
            if !got.is_empty() {
                break;
            }
        }
        assert!(!got.is_empty(), "nothing delivered inside the window");
        assert!(got.iter().all(|d| d.len() % 188 == 0 && d[0] == 0x47), "MPEG-TS");

        // The window closes: the push stops while the stream stays live.
        tokio::time::advance(Duration::from_secs(3600)).await;
        until("the window to close", || auto.rules()[0].active.is_empty()).await;
        assert_eq!(auto.rules()[0].idle.as_ref().unwrap().reason, "outside_window");
        assert!(auto.inner.state.lock().applied.udp.is_empty());
        received(&sock, Duration::from_millis(200)).await; // drain
        feed(&cam, 2000, 100);
        assert!(received(&sock, Duration::from_millis(300)).await.is_empty(), "nothing after the window");
        assert!(!cam.stream().is_ended());
    }

    /// A recurring window (cron) reopens the next day; a reload that only
    /// moves the window leaves an unrelated static push untouched.
    #[tokio::test(start_paused = true)]
    async fn cron_window_recurs_and_reload_moves_it() {
        let registry = Registry::new();
        let (restream, srt, udp) = engines(&registry);
        let stat = caudal_restream::RestreamTarget { stream: "s".into(), url: "rtmp://x/a/k".into() };
        let daily = |cron: &str| AutoPushEntry {
            cron: Some(cron.into()),
            tz: Some("America/Puerto_Rico".into()),
            duration_secs: Some(3600),
            ..rule(&["show"], "srt://h:9000?streamid=$stream")
        };
        let set = PushSet { restream: vec![stat.clone()], rules: vec![daily("0 20 * * *")], ..Default::default() };
        let clock = test_clock("2026-12-24T19:30:00-04:00");
        let whip = caudal_webrtc::start_pushes(registry.clone(), vec![]);
        let auto = AutoPush::start_with_clock(registry.clone(), set.clone(), restream, srt, udp, whip, clock);
        let _p = registry.publish("show", caudal_core::BufferConfig::default()).unwrap();
        let srt_len = |a: &AutoPush| a.inner.state.lock().applied.srt.len();
        until("the publish to be seen", || auto.rules()[0].idle.as_ref().unwrap().reason == "outside_window").await;
        tokio::time::advance(Duration::from_secs(30 * 60)).await;
        until("20:00", || srt_len(&auto) == 1).await;
        tokio::time::advance(Duration::from_secs(3600)).await;
        until("21:00", || srt_len(&auto) == 0).await;
        // Next day, 20:00 again (23 hours later).
        tokio::time::advance(Duration::from_secs(23 * 3600)).await;
        until("the next day's window", || srt_len(&auto) == 1).await;

        // Move the window to 22:00: closes now; the static push is never
        // reloaded (its engine list is unchanged).
        let restream_before = auto.inner.state.lock().applied.restream.clone();
        auto.configure(PushSet { rules: vec![daily("0 22 * * *")], ..set });
        assert_eq!(srt_len(&auto), 0);
        assert_eq!(auto.inner.state.lock().applied.restream, restream_before);
        assert_eq!(restream_before, [stat]);
        let w = auto.rules()[0].schedule.clone().unwrap().window.unwrap();
        assert_eq!(w.start.as_deref(), Some("2026-12-26T02:00:00Z"));
        tokio::time::advance(Duration::from_secs(2 * 3600)).await;
        until("22:00", || srt_len(&auto) == 1).await;
    }
}
