//! Stream health alerts: one watcher over the registry that notices when a
//! live stream stops sending keyframes, drops below a bitrate floor, loses
//! its publisher, or (optionally) goes quiet on audio, and fires a signed
//! webhook — clearing it with a `resolved` once the stream recovers.
//!
//! Entry points fixed by the orchestrator: [`start`] wires a [`HealthConfig`]
//! to a `caudal_core::Registry`; [`HealthService::router`] mounts
//! `GET /api/v1/alerts`.
//!
//! ## How rules are evaluated
//! One 1-second tick ([`TICK`]) walks every stream this crate has ever seen
//! publish and asks each enabled rule "is this bad right now". A per-stream,
//! per-rule [`rules::RuleState`] turns that into at most one webhook: an
//! `alert` the instant it turns bad, and one `resolved` only after the
//! condition has been continuously good for `min_hold_secs` (hysteresis, so
//! a value bouncing around a threshold does not flap).
//!
//! ## `publisher_lost`: what "unexpected" means here
//! `caudal-core`'s [`caudal_core::Publisher`] is dropped identically whether
//! ingest closed the connection cleanly or the network just died — there is
//! no separate "clean unpublish" signal anywhere in the core to distinguish
//! them (see `Publisher`'s `Drop` impl: it always calls `Stream::end` and
//! sends on `Registry::subscribe_ends`). So this crate does not try to
//! guess intent from the disconnect itself; instead it treats every
//! disappearance as *possibly* a planned restart and gives it
//! `publisher_lost_grace_secs` to republish under the same name before
//! alerting. A quick OBS reconnect never alerts; a publisher that is
//! actually gone does, after the grace window.
//!
//! ## Keyframe/audio timing needs real-time observation
//! `Stream::stats()` only exposes cumulative counters, not "when did the
//! last keyframe arrive", so this crate subscribes to each live stream as an
//! internal (uncounted) consumer — the same mechanism `caudal-record` uses
//! — purely to timestamp keyframe and audio frame arrivals in wall-clock
//! time. Bitrate reuses the existing `bytes_in` counter instead (a per-tick
//! delta), since that one *is* already exposed.

mod delivery;
mod http;
mod rules;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use caudal_core::{Event, Registry, StartAt, Stream, TrackKind};
use parking_lot::Mutex;
use rules::{BitrateTracker, Observation, RuleKind, RuleState, SinceTracker, Transition};
use tokio::sync::broadcast;
use tokio::time::Instant;

pub use delivery::AlertPayload;
pub use http::{ActiveAlertJson, AlertEventJson};

/// How often rules are evaluated. Fixed, not configurable: the brief asks
/// for a 1 s tick, and a configurable one would need to double as the unit
/// used to reason about `for_secs`/`*_secs` thresholds, which is not worth
/// the surface area.
const TICK: Duration = Duration::from_secs(1);

/// Global defaults plus optional per-stream overrides. Built by
/// `crates/caudal/src/config.rs` from `[health]` / `[[health.stream]]`.
#[derive(Debug, Clone)]
pub struct HealthConfig {
    /// `None` disables the rule.
    pub no_keyframe_secs: Option<u64>,
    /// `None` disables the rule.
    pub min_bitrate_kbps: Option<u32>,
    pub min_bitrate_for_secs: u64,
    /// `None` disables the rule. Only ever evaluated for streams that
    /// declare an audio track; a video-only stream never trips it.
    pub no_audio_secs: Option<u64>,
    pub publisher_lost: bool,
    pub publisher_lost_grace_secs: u64,
    /// Hysteresis: how long a rule must be continuously good before its
    /// `resolved` fires.
    pub min_hold_secs: u64,
    pub webhooks: Vec<String>,
    /// Standard Webhooks secret (`whsec_...`).
    pub secret: String,
    pub overrides: Vec<StreamOverride>,
}

/// `[[health.stream]]`: replaces the matching default for one stream name.
/// A field left `None` inherits the global default (including "disabled"
/// when the default itself is `None`).
#[derive(Debug, Clone, Default)]
pub struct StreamOverride {
    pub name: String,
    pub no_keyframe_secs: Option<u64>,
    pub min_bitrate_kbps: Option<u32>,
    pub min_bitrate_for_secs: Option<u64>,
    pub no_audio_secs: Option<u64>,
    pub publisher_lost: Option<bool>,
    pub publisher_lost_grace_secs: Option<u64>,
    pub min_hold_secs: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
struct Resolved {
    no_keyframe_secs: Option<u64>,
    min_bitrate_kbps: Option<u32>,
    min_bitrate_for_secs: u64,
    no_audio_secs: Option<u64>,
    publisher_lost: bool,
    publisher_lost_grace_secs: u64,
    min_hold_secs: u64,
}

impl HealthConfig {
    fn resolve(&self, stream: &str) -> Resolved {
        let o = self.overrides.iter().find(|o| o.name == stream);
        Resolved {
            no_keyframe_secs: o.and_then(|o| o.no_keyframe_secs).or(self.no_keyframe_secs),
            min_bitrate_kbps: o.and_then(|o| o.min_bitrate_kbps).or(self.min_bitrate_kbps),
            min_bitrate_for_secs: o.and_then(|o| o.min_bitrate_for_secs).unwrap_or(self.min_bitrate_for_secs),
            no_audio_secs: o.and_then(|o| o.no_audio_secs).or(self.no_audio_secs),
            publisher_lost: o.and_then(|o| o.publisher_lost).unwrap_or(self.publisher_lost),
            publisher_lost_grace_secs: o
                .and_then(|o| o.publisher_lost_grace_secs)
                .unwrap_or(self.publisher_lost_grace_secs),
            min_hold_secs: o.and_then(|o| o.min_hold_secs).unwrap_or(self.min_hold_secs),
        }
    }
}

/// Per-stream tracking. Survives the stream ending, so `publisher_lost`'s
/// grace window and hold time can still be evaluated with nothing left to
/// subscribe to.
struct Entry {
    resolved: Resolved,
    alive: bool,
    has_audio: bool,
    last_keyframe: SinceTracker,
    last_audio: SinceTracker,
    /// Set the instant the publisher goes away; cleared on republish.
    lost_since: Option<Instant>,
    bitrate: BitrateTracker,
    last_bytes_in: u64,
    rule_no_keyframe: RuleState,
    rule_min_bitrate: RuleState,
    rule_no_audio: RuleState,
    rule_publisher_lost: RuleState,
}

impl Entry {
    fn new(resolved: Resolved, now: Instant) -> Self {
        Self {
            resolved,
            alive: true,
            has_audio: false,
            last_keyframe: SinceTracker::seeded(now),
            last_audio: SinceTracker::unset(),
            lost_since: None,
            bitrate: BitrateTracker::new(),
            last_bytes_in: 0,
            rule_no_keyframe: RuleState::new(),
            rule_min_bitrate: RuleState::new(),
            rule_no_audio: RuleState::new(),
            rule_publisher_lost: RuleState::new(),
        }
    }

    fn any_rule_active(&self) -> bool {
        self.rule_no_keyframe.is_active()
            || self.rule_min_bitrate.is_active()
            || self.rule_no_audio.is_active()
            || self.rule_publisher_lost.is_active()
    }
}

#[derive(Debug, Clone)]
struct ActiveInfo {
    value: f64,
    threshold: f64,
    since: String,
}

#[derive(Debug, Clone)]
struct EventLog {
    event: &'static str,
    rule: &'static str,
    stream: String,
    value: f64,
    threshold: f64,
    at: String,
}

const EVENTS_KEPT: usize = 100;

struct Shared {
    registry: Arc<Registry>,
    cfg: HealthConfig,
    entries: Mutex<HashMap<String, Entry>>,
    /// One task watches frames per live stream; guards against subscribing
    /// twice to the same publish (mirrors `caudal-record`'s `recorders`).
    watching: Mutex<HashMap<String, Arc<Stream>>>,
    active: Mutex<HashMap<(String, &'static str), ActiveInfo>>,
    events: Mutex<std::collections::VecDeque<EventLog>>,
    fired_total: [AtomicU64; 4],
    delivery: delivery::Delivery,
}

fn rule_index(kind: RuleKind) -> usize {
    match kind {
        RuleKind::NoKeyframe => 0,
        RuleKind::MinBitrate => 1,
        RuleKind::PublisherLost => 2,
        RuleKind::NoAudio => 3,
    }
}

pub struct HealthService {
    shared: Arc<Shared>,
}

/// One rule's snapshot for `/metrics`.
pub struct RuleMetrics {
    pub rule: &'static str,
    pub active: usize,
    pub fired_total: u64,
}

impl HealthService {
    pub fn router(&self) -> axum::Router {
        http::router(self.shared.clone())
    }

    /// Sends a `failover_switched` webhook (a backup-source switch from
    /// `caudal-failover`) and lists it in `GET /api/v1/alerts`' events.
    /// Not a rule: it never becomes an active alert.
    pub fn failover_switched(&self, stream: &str, from: Option<&str>, to: &str, reason: &'static str) {
        let payload = delivery::AlertPayload::failover_switched(stream, from, to, reason);
        let mut events = self.shared.events.lock();
        events.push_back(EventLog {
            event: payload.event,
            rule: payload.rule,
            stream: stream.to_owned(),
            value: 0.0,
            threshold: 0.0,
            at: payload.at.clone(),
        });
        while events.len() > EVENTS_KEPT {
            events.pop_front();
        }
        drop(events);
        self.shared.delivery.send(payload);
    }

    /// Active-alert count and lifetime fire count per rule, for
    /// `caudal_alerts_active{rule}` / `caudal_alerts_fired_total{rule}`.
    pub fn metrics(&self) -> Vec<RuleMetrics> {
        let active = self.shared.active.lock();
        RuleKind::ALL
            .iter()
            .map(|&kind| RuleMetrics {
                rule: kind.as_str(),
                active: active.keys().filter(|(_, r)| *r == kind.as_str()).count(),
                fired_total: self.shared.fired_total[rule_index(kind)].load(Ordering::Relaxed),
            })
            .collect()
    }
}

/// Starts the health watcher. Must be called inside a tokio runtime.
/// Fails only if `cfg.secret` cannot build a Standard Webhooks signer.
pub fn start(registry: Arc<Registry>, cfg: HealthConfig) -> Result<HealthService, String> {
    let delivery = delivery::Delivery::start(cfg.webhooks.clone(), &cfg.secret)?;
    let shared = Arc::new(Shared {
        registry: registry.clone(),
        cfg,
        entries: Mutex::default(),
        watching: Mutex::default(),
        active: Mutex::default(),
        events: Mutex::new(std::collections::VecDeque::with_capacity(EVENTS_KEPT)),
        fired_total: [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)],
        delivery,
    });

    // Subscribe before listing, so a publish landing in between is still
    // seen at least once (`spawn_watcher` ignores duplicates either way).
    let publishes = registry.subscribe_publishes();
    for stream in registry.list() {
        spawn_watcher(&shared, stream);
    }
    tokio::spawn(listen(shared.clone(), publishes));
    tokio::spawn(tick_loop(shared.clone()));

    Ok(HealthService { shared })
}

async fn listen(shared: Arc<Shared>, mut publishes: broadcast::Receiver<Arc<Stream>>) {
    loop {
        match publishes.recv().await {
            Ok(stream) => spawn_watcher(&shared, stream),
            Err(broadcast::error::RecvError::Lagged(_)) => {
                for stream in shared.registry.list() {
                    spawn_watcher(&shared, stream);
                }
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

fn spawn_watcher(shared: &Arc<Shared>, stream: Arc<Stream>) {
    let name = stream.name().to_owned();
    {
        let mut watching = shared.watching.lock();
        if watching.get(&name).is_some_and(|s| Arc::ptr_eq(s, &stream)) {
            return;
        }
        watching.insert(name.clone(), stream.clone());
    }

    let now = Instant::now();
    {
        let mut entries = shared.entries.lock();
        let resolved = shared.cfg.resolve(&name);
        let entry = entries.entry(name.clone()).or_insert_with(|| Entry::new(resolved, now));
        entry.resolved = resolved;
        entry.alive = true;
        entry.lost_since = None;
        entry.has_audio = false;
        entry.last_keyframe = SinceTracker::seeded(now);
        entry.last_audio = SinceTracker::unset();
        entry.bitrate = BitrateTracker::new();
        entry.last_bytes_in = 0;
        // Rule states are intentionally left as they were: a stream that
        // reconnects while an alert is active should only clear it by
        // actually proving healthy again, not by the act of reconnecting.
    }

    // Not a viewer: `subscribe_internal` (see caudal-core's Stream docs).
    let sub = stream.subscribe_internal(StartAt::LiveEdge);
    tracing::debug!(stream = %name, "health watcher started");
    tokio::spawn(watch_frames(shared.clone(), stream, sub));
}

async fn watch_frames(shared: Arc<Shared>, stream: Arc<Stream>, mut sub: caudal_core::Subscriber) {
    let name = stream.name().to_owned();
    let mut tracks: Vec<caudal_core::TrackInfo> = Vec::new();
    loop {
        let ev = sub.recv().await;
        let ended = ev == Event::End;
        match ev {
            Event::TracksChanged => {
                tracks = sub.tracks();
                if tracks.iter().any(|t| t.kind() == TrackKind::Audio) {
                    let now = Instant::now();
                    let mut entries = shared.entries.lock();
                    if let Some(e) = entries.get_mut(&name)
                        && !e.has_audio
                    {
                        e.has_audio = true;
                        e.last_audio = SinceTracker::seeded(now);
                    }
                }
            }
            Event::Frame(f) => {
                let kind = tracks.iter().find(|t| t.id == f.track).map(|t| t.kind());
                let now = Instant::now();
                let mut entries = shared.entries.lock();
                if let Some(e) = entries.get_mut(&name) {
                    match kind {
                        Some(TrackKind::Video) if f.keyframe => e.last_keyframe.mark(now),
                        Some(TrackKind::Audio) => e.last_audio.mark(now),
                        _ => {}
                    }
                }
            }
            Event::Lagged { .. } | Event::Cue(_) => {}
            Event::End => {
                let mut entries = shared.entries.lock();
                if let Some(e) = entries.get_mut(&name) {
                    e.alive = false;
                    e.lost_since = Some(Instant::now());
                }
            }
        }
        if ended {
            break;
        }
    }
    let mut watching = shared.watching.lock();
    if watching.get(&name).is_some_and(|s| Arc::ptr_eq(s, &stream)) {
        watching.remove(&name);
    }
}

async fn tick_loop(shared: Arc<Shared>) {
    let mut interval = tokio::time::interval(TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        tick_once(&shared);
    }
}

fn lost_observation(lost_since: Option<Instant>, now: Instant, grace: Duration) -> Observation {
    match lost_since {
        None => Observation { is_bad: false, value: 0.0, threshold: grace.as_secs_f64() },
        Some(since) => {
            let elapsed = now.saturating_duration_since(since);
            Observation { is_bad: elapsed >= grace, value: elapsed.as_secs_f64(), threshold: grace.as_secs_f64() }
        }
    }
}

/// One evaluation pass over every tracked stream. Split out from
/// [`tick_loop`] so tests can drive it directly under a paused clock.
fn tick_once(shared: &Shared) {
    let now = Instant::now();
    let mut fired: Vec<(String, RuleKind, Transition)> = Vec::new();
    let mut to_remove: Vec<String> = Vec::new();

    {
        let mut entries = shared.entries.lock();
        for (name, e) in entries.iter_mut() {
            let hold = Duration::from_secs(e.resolved.min_hold_secs);

            if let Some(secs) = e.resolved.no_keyframe_secs {
                let obs = if e.alive {
                    e.last_keyframe.observe(now, Duration::from_secs(secs))
                } else {
                    Observation { is_bad: false, value: 0.0, threshold: secs as f64 }
                };
                if let Some(t) = e.rule_no_keyframe.tick(now, obs, hold) {
                    fired.push((name.clone(), RuleKind::NoKeyframe, t));
                }
            }

            if let Some(floor) = e.resolved.min_bitrate_kbps {
                let obs = if e.alive {
                    let bytes_in = shared.registry.get(name).map(|s| s.stats().bytes_in).unwrap_or(e.last_bytes_in);
                    let delta = bytes_in.saturating_sub(e.last_bytes_in);
                    e.last_bytes_in = bytes_in;
                    // One tick is `TICK` seconds; bytes -> kbps over it.
                    let kbps = (delta as f64) * 8.0 / 1000.0 / TICK.as_secs_f64();
                    let for_secs = Duration::from_secs(e.resolved.min_bitrate_for_secs.max(1));
                    e.bitrate.sample(now, kbps, floor as f64, for_secs)
                } else {
                    e.bitrate.reset();
                    Observation { is_bad: false, value: 0.0, threshold: floor as f64 }
                };
                if let Some(t) = e.rule_min_bitrate.tick(now, obs, hold) {
                    fired.push((name.clone(), RuleKind::MinBitrate, t));
                }
            }

            if e.has_audio
                && let Some(secs) = e.resolved.no_audio_secs
            {
                let obs = if e.alive {
                    e.last_audio.observe(now, Duration::from_secs(secs))
                } else {
                    Observation { is_bad: false, value: 0.0, threshold: secs as f64 }
                };
                if let Some(t) = e.rule_no_audio.tick(now, obs, hold) {
                    fired.push((name.clone(), RuleKind::NoAudio, t));
                }
            }

            if e.resolved.publisher_lost {
                let grace = Duration::from_secs(e.resolved.publisher_lost_grace_secs);
                let obs = lost_observation(e.lost_since, now, grace);
                if let Some(t) = e.rule_publisher_lost.tick(now, obs, hold) {
                    fired.push((name.clone(), RuleKind::PublisherLost, t));
                }
            }

            // Only prune once there is truly nothing left to track: still
            // alive entries are never pruned, and a dead one is kept as long
            // as publisher_lost tracking wants it (its grace timer, or an
            // alert only a republish can resolve) or any other rule is
            // still active.
            if !e.alive && !e.resolved.publisher_lost && !e.any_rule_active() {
                to_remove.push(name.clone());
            }
        }
        for name in &to_remove {
            entries.remove(name);
        }
    }

    for (name, kind, transition) in fired {
        handle_transition(shared, &name, kind, transition);
    }
}

fn handle_transition(shared: &Shared, stream: &str, kind: RuleKind, transition: Transition) {
    let (event, value, threshold) = match transition {
        Transition::Alert { value, threshold } => ("alert", value, threshold),
        Transition::Resolved { value, threshold } => ("resolved", value, threshold),
    };
    let payload = delivery::AlertPayload::new(event, kind.as_str(), stream, value, threshold);
    let key = (stream.to_owned(), kind.as_str());
    if event == "alert" {
        shared.fired_total[rule_index(kind)].fetch_add(1, Ordering::Relaxed);
        shared.active.lock().insert(key, ActiveInfo { value, threshold, since: payload.at.clone() });
        tracing::warn!(stream, rule = kind.as_str(), value, threshold, "stream health alert");
    } else {
        shared.active.lock().remove(&key);
        tracing::info!(stream, rule = kind.as_str(), value, threshold, "stream health alert resolved");
    }

    let mut events = shared.events.lock();
    events.push_back(EventLog {
        event,
        rule: kind.as_str(),
        stream: stream.to_owned(),
        value,
        threshold,
        at: payload.at.clone(),
    });
    while events.len() > EVENTS_KEPT {
        events.pop_front();
    }
    drop(events);

    shared.delivery.send(payload);
}

#[cfg(test)]
mod tests {
    use super::*;
    use caudal_core::{AudioParams, BufferConfig, Codec, Frame, TrackId, TrackInfo, VideoParams};

    fn base_config(webhooks: Vec<String>) -> HealthConfig {
        HealthConfig {
            no_keyframe_secs: Some(3),
            min_bitrate_kbps: None,
            min_bitrate_for_secs: 3,
            no_audio_secs: None,
            publisher_lost: true,
            publisher_lost_grace_secs: 2,
            min_hold_secs: 2,
            webhooks,
            secret: "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw".to_string(),
            overrides: Vec::new(),
        }
    }

    #[test]
    fn resolve_applies_per_stream_override() {
        let mut cfg = base_config(vec![]);
        cfg.overrides.push(StreamOverride {
            name: "live-main".into(),
            no_keyframe_secs: Some(1),
            ..Default::default()
        });
        let default = cfg.resolve("live-other");
        assert_eq!(default.no_keyframe_secs, Some(3));
        let overridden = cfg.resolve("live-main");
        assert_eq!(overridden.no_keyframe_secs, Some(1));
        // Everything not overridden still inherits the default.
        assert_eq!(overridden.min_hold_secs, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn no_keyframe_alert_and_resolve_end_to_end() {
        let registry = Registry::new();
        let svc = start(registry.clone(), base_config(vec![])).unwrap();

        let publisher = registry.publish("live-main", BufferConfig::default()).unwrap();
        publisher
            .set_tracks(vec![TrackInfo {
                id: TrackId(0),
                codec: Codec::H264,
                timescale: 90_000,
                init: Default::default(),
                lang: None,
                video: Some(VideoParams { width: 16, height: 16, fps: None }),
                audio: None,
            }])
            .unwrap();
        // Let the watcher task observe TracksChanged before we drive time.
        tokio::task::yield_now().await;

        publisher.push(Frame { track: TrackId(0), dts: 0, pts: 0, keyframe: true, data: Default::default() }).unwrap();
        tokio::task::yield_now().await;

        // No keyframe for 3s: must alert once.
        tokio::time::advance(Duration::from_secs(4)).await;
        tokio::task::yield_now().await;
        assert_eq!(svc.metrics().iter().find(|m| m.rule == "no_keyframe").unwrap().active, 1);
        assert_eq!(svc.metrics().iter().find(|m| m.rule == "no_keyframe").unwrap().fired_total, 1);

        // A healthy stream sending a keyframe every second (well under the
        // 3s threshold) held for min_hold (2s): must resolve, exactly once.
        for i in 1..=4u32 {
            let ts = i64::from(i) * 90_000;
            publisher
                .push(Frame { track: TrackId(0), dts: ts, pts: ts, keyframe: true, data: Default::default() })
                .unwrap();
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
        }
        assert_eq!(svc.metrics().iter().find(|m| m.rule == "no_keyframe").unwrap().active, 0);
        assert_eq!(svc.metrics().iter().find(|m| m.rule == "no_keyframe").unwrap().fired_total, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn publisher_lost_waits_out_the_grace_window() {
        let registry = Registry::new();
        let svc = start(registry.clone(), base_config(vec![])).unwrap();

        let publisher = registry.publish("live-main", BufferConfig::default()).unwrap();
        tokio::task::yield_now().await;
        drop(publisher);
        tokio::task::yield_now().await;

        // Republish inside the grace window: no alert should ever fire.
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        let _p2 = registry.publish("live-main", BufferConfig::default()).unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::task::yield_now().await;
        assert_eq!(svc.metrics().iter().find(|m| m.rule == "publisher_lost").unwrap().fired_total, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn publisher_lost_fires_when_it_never_comes_back() {
        let registry = Registry::new();
        let svc = start(registry.clone(), base_config(vec![])).unwrap();

        let publisher = registry.publish("live-main", BufferConfig::default()).unwrap();
        tokio::task::yield_now().await;
        drop(publisher);
        tokio::task::yield_now().await;

        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::task::yield_now().await;
        assert_eq!(svc.metrics().iter().find(|m| m.rule == "publisher_lost").unwrap().fired_total, 1);
        assert_eq!(svc.metrics().iter().find(|m| m.rule == "publisher_lost").unwrap().active, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn no_audio_only_applies_once_an_audio_track_is_declared() {
        let registry = Registry::new();
        let mut cfg = base_config(vec![]);
        cfg.no_keyframe_secs = None;
        cfg.no_audio_secs = Some(2);
        let svc = start(registry.clone(), cfg).unwrap();

        let publisher = registry.publish("cam", BufferConfig::default()).unwrap();
        publisher
            .set_tracks(vec![TrackInfo {
                id: TrackId(0),
                codec: Codec::H264,
                timescale: 90_000,
                init: Default::default(),
                lang: None,
                video: Some(VideoParams { width: 16, height: 16, fps: None }),
                audio: None,
            }])
            .unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            svc.metrics().iter().find(|m| m.rule == "no_audio").unwrap().fired_total,
            0,
            "video-only never trips no_audio"
        );

        let publisher2 = registry.publish("mic", BufferConfig::default()).unwrap();
        publisher2
            .set_tracks(vec![TrackInfo {
                id: TrackId(0),
                codec: Codec::Aac,
                timescale: 48_000,
                init: Default::default(),
                lang: None,
                video: None,
                audio: Some(AudioParams { sample_rate: 48_000, channels: 2 }),
            }])
            .unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::task::yield_now().await;
        assert_eq!(svc.metrics().iter().find(|m| m.rule == "no_audio").unwrap().fired_total, 1);
    }
}
