//! Backup sources (PLAN batch 10): a public stream that keeps playing when
//! its primary source dies.
//!
//! A `[[failover]]` entry names a public stream (`main`) and an ordered
//! list of sources: ordinary ingest names (any protocol: RTMP, SRT, WHIP,
//! an RTSP pull) or `file:<path>` (a looping file played by
//! `caudal-channel`'s player). The public stream is a derived output: one
//! task subscribes to whichever source is active and republishes its
//! frames under the public name, so every output (LL-HLS, WHEP, MoQ,
//! recording, restream, transcode) serves it like any other stream and a
//! switch never restarts a player.
//!
//! - **Health**: a stream source is healthy while its frame counter moved
//!   within `switch_after` (published or just dropped). A file source is healthy
//!   while its path exists.
//! - **Switching** (rules in `switcher.rs`): the active source silent for
//!   `switch_after` hands over to the best other healthy source; a
//!   better-ranked source healthy for `switch_back_after` takes back over.
//!   A publisher that drops and republishes the active name within
//!   `switch_after` is picked up again without a switch.
//! - **Keyframes**: the new source is joined at its newest buffered
//!   keyframe (`StartAt::LiveEdge`), so the first frame after a switch
//!   decodes on its own.
//! - **Timestamps** (`stitch.rs`) stay monotonic: each switch continues
//!   where the output left off.
//! - **Codec changes**: a source whose track list differs from the one on
//!   air replaces the output's track list, which viewers receive as
//!   `Event::TracksChanged` (LL-HLS writes a new `EXT-X-MAP` after an
//!   `EXT-X-DISCONTINUITY`). Identical track lists change nothing.
//! - **File sources** run only while they are on air, published under the
//!   internal name `failover.<stream>.<index>` (visible in the stream list
//!   while it plays).
//!
//! Routes (absolute, merged at the root):
//! - `GET /api/v1/failover` → JSON array of [`FailoverStatus`]
//! - `POST /api/v1/failover/{stream}/switch` with `{"source": "<name>"}`
//!   switches now and prefers that source (no automatic switch back while
//!   it is healthy); `{"source": null}` returns to automatic. 204, 400
//!   (bad body or unknown source), 404 (unknown stream), 409 (source not
//!   healthy). Asks `Registry::authorize(Publish, stream)`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use caudal_core::media::valid_stream_name;
use caudal_core::{BufferConfig, Cue, Event, PublishError, Publisher, Registry, StartAt, Stream, Subscriber};
use parking_lot::Mutex;
use serde::Serialize;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::Instant;

mod http;
mod stitch;
mod switcher;

use stitch::Stitcher;
pub use switcher::Reason;
use switcher::Switcher;

/// How often source health is sampled.
pub const TICK: Duration = Duration::from_millis(100);
/// Switch events kept in each stream's status.
const HISTORY: usize = 20;

#[derive(Debug, Clone, Default)]
pub struct FailoverConfig {
    pub entries: Vec<Failover>,
    pub buffer: BufferConfig,
    /// `[server] trusted_proxies`, for resolving `X-Forwarded-For` on the
    /// manual switch route; see `caudal_core::net::resolve_forwarded`. Fixed
    /// for the process lifetime (`[server]` requires a restart).
    pub trusted_proxies: Vec<caudal_core::Cidr>,
}

/// One `[[failover]]` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failover {
    /// Public stream name viewers play.
    pub stream: String,
    /// In priority order.
    pub sources: Vec<Source>,
    /// No frames from the active source for this long: switch.
    pub switch_after: Duration,
    /// A better-ranked source healthy this long: switch back.
    pub switch_back_after: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// An ingest stream name.
    Stream(String),
    /// A media file played in a loop.
    File(PathBuf),
}

impl Source {
    /// `file:<path>` or a stream name.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.strip_prefix("file:") {
            Some("") => Err("`file:` needs a path".into()),
            Some(p) => Ok(Source::File(PathBuf::from(p))),
            None if valid_stream_name(s) => Ok(Source::Stream(s.to_owned())),
            None => Err(format!("`{s}` is neither a valid stream name nor `file:<path>`")),
        }
    }

    /// As written in the config.
    pub fn label(&self) -> String {
        match self {
            Source::Stream(n) => n.clone(),
            Source::File(p) => format!("file:{}", p.display()),
        }
    }
}

impl Failover {
    /// Checks one entry on its own (names, sources, timings).
    pub fn validate(&self) -> Result<(), String> {
        if !valid_stream_name(&self.stream) {
            return Err(format!("[[failover]] stream `{}` is not a valid stream name", self.stream));
        }
        if self.sources.is_empty() {
            return Err(format!("[[failover]] `{}` needs at least one source", self.stream));
        }
        if self.switch_after.is_zero() {
            return Err(format!("[[failover]] `{}`: switch_after_ms must be above 0", self.stream));
        }
        for (i, s) in self.sources.iter().enumerate() {
            match s {
                Source::Stream(n) if *n == self.stream => {
                    return Err(format!("[[failover]] `{}` cannot list itself as a source", self.stream));
                }
                Source::File(_) if !valid_stream_name(&file_stream_name(&self.stream, i)) => {
                    return Err(format!("[[failover]] stream name `{}` is too long for a file source", self.stream));
                }
                _ => {}
            }
            if self.sources[..i].contains(s) {
                return Err(format!("[[failover]] `{}` lists `{}` twice", self.stream, s.label()));
            }
        }
        Ok(())
    }
}

/// The internal stream a file source plays under while it is on air.
pub fn file_stream_name(stream: &str, index: usize) -> String {
    format!("failover.{stream}.{index}")
}

/// A switch, as served by the API and sent to subscribers.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SwitchEvent {
    pub stream: String,
    /// Source on air before, if any.
    pub from: Option<String>,
    pub to: String,
    /// `start`, `silent`, `recovered` or `manual`.
    pub reason: &'static str,
    /// Unix time in milliseconds.
    pub at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SourceStatus {
    /// As configured (`name` or `file:<path>`).
    pub name: String,
    /// `stream` or `file`.
    pub kind: &'static str,
    /// A stream source is published; a file source is playing.
    pub live: bool,
    pub healthy: bool,
    /// Milliseconds since this source last sent a frame (stream sources
    /// seen at least once).
    pub silent_ms: Option<u64>,
    /// Milliseconds this source has been healthy without a break.
    pub healthy_for_ms: Option<u64>,
}

/// Status of one failover stream, as served by `GET /api/v1/failover`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FailoverStatus {
    pub stream: String,
    /// The source on air (or waiting to be joined), as configured.
    pub active: Option<String>,
    /// Set by a manual switch: this source outranks the others.
    pub preferred: Option<String>,
    /// The public stream is published.
    pub on_air: bool,
    pub sources: Vec<SourceStatus>,
    pub last_switch: Option<SwitchEvent>,
    pub switches: u64,
    /// The most recent switches, oldest first.
    pub history: Vec<SwitchEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitchError {
    UnknownStream,
    UnknownSource,
    NotHealthy,
}

type Command = (Option<String>, oneshot::Sender<Result<(), SwitchError>>);

struct Shared {
    status: Mutex<FailoverStatus>,
    cmds: mpsc::Sender<Command>,
}

struct Entry {
    cfg: Failover,
    shared: Arc<Shared>,
    task: tokio::task::JoinHandle<()>,
}

/// Running failovers. Cheap to clone. Dropping it does not stop them
/// (see [`FailoverHandle::stop`]).
#[derive(Clone)]
pub struct FailoverHandle {
    inner: Arc<Inner>,
}

impl FailoverHandle {
    pub(crate) fn trusted_proxies(&self) -> &[caudal_core::Cidr] {
        &self.inner.trusted_proxies
    }
}

struct Inner {
    registry: Arc<Registry>,
    buffer: BufferConfig,
    entries: Mutex<Vec<Entry>>,
    switches: broadcast::Sender<SwitchEvent>,
    trusted_proxies: Vec<caudal_core::Cidr>,
}

fn initial_status(cfg: &Failover) -> FailoverStatus {
    FailoverStatus {
        stream: cfg.stream.clone(),
        active: None,
        preferred: None,
        on_air: false,
        sources: cfg
            .sources
            .iter()
            .map(|s| SourceStatus {
                name: s.label(),
                kind: match s {
                    Source::Stream(_) => "stream",
                    Source::File(_) => "file",
                },
                live: false,
                healthy: false,
                silent_ms: None,
                healthy_for_ms: None,
            })
            .collect(),
        last_switch: None,
        switches: 0,
        history: Vec::new(),
    }
}

fn spawn_entry(inner: &Inner, cfg: Failover) -> Entry {
    let (tx, rx) = mpsc::channel(8);
    let shared = Arc::new(Shared { status: Mutex::new(initial_status(&cfg)), cmds: tx });
    let runner = Runner::new(cfg.clone(), inner.registry.clone(), inner.buffer, shared.clone(), inner.switches.clone());
    let task = tokio::spawn(runner.run(rx));
    Entry { cfg, shared, task }
}

impl FailoverHandle {
    /// Every failover's status, in configuration order.
    pub fn status(&self) -> Vec<FailoverStatus> {
        self.inner.entries.lock().iter().map(|e| e.shared.status.lock().clone()).collect()
    }

    /// Every switch from now on, on every failover stream.
    pub fn subscribe_switches(&self) -> broadcast::Receiver<SwitchEvent> {
        self.inner.switches.subscribe()
    }

    /// Manual switch: `Some(source)` puts that source on air now and
    /// prefers it; `None` returns to automatic priority order.
    pub async fn switch(&self, stream: &str, source: Option<String>) -> Result<(), SwitchError> {
        let shared = self.find(stream).ok_or(SwitchError::UnknownStream)?;
        let (tx, rx) = oneshot::channel();
        shared.cmds.send((source, tx)).await.map_err(|_| SwitchError::UnknownStream)?;
        rx.await.map_err(|_| SwitchError::UnknownStream)?
    }

    /// Stops every failover task; their public streams end.
    pub fn stop(&self) {
        for e in self.inner.entries.lock().drain(..) {
            e.task.abort();
        }
    }

    /// Applies a new list: an unchanged entry keeps running untouched (its
    /// viewers never notice the reload); a changed or new one is
    /// (re)started; a dropped one is stopped, ending its public stream.
    pub fn reload(&self, cfg: FailoverConfig) {
        let mut entries = self.inner.entries.lock();
        let mut remaining = std::mem::take(&mut *entries);
        let mut next = Vec::with_capacity(cfg.entries.len());
        for f in cfg.entries {
            if let Some(pos) = remaining.iter().position(|e| e.cfg == f) {
                next.push(remaining.remove(pos));
                continue;
            }
            if let Some(pos) = remaining.iter().position(|e| e.cfg.stream == f.stream) {
                remaining.remove(pos).task.abort();
            }
            next.push(spawn_entry(&self.inner, f));
        }
        for gone in remaining {
            gone.task.abort();
        }
        *entries = next;
    }

    fn find(&self, stream: &str) -> Option<Arc<Shared>> {
        self.inner.entries.lock().iter().find(|e| e.cfg.stream == stream).map(|e| e.shared.clone())
    }

    pub(crate) fn has(&self, stream: &str) -> bool {
        self.find(stream).is_some()
    }

    pub(crate) fn registry(&self) -> &Arc<Registry> {
        &self.inner.registry
    }
}

/// Spawns one task per failover on the current tokio runtime.
pub fn start(registry: Arc<Registry>, cfg: FailoverConfig) -> FailoverHandle {
    let inner = Arc::new(Inner {
        registry,
        buffer: cfg.buffer,
        entries: Mutex::new(Vec::new()),
        switches: broadcast::channel(64).0,
        trusted_proxies: cfg.trusted_proxies,
    });
    let entries = cfg.entries.into_iter().map(|f| spawn_entry(&inner, f)).collect();
    *inner.entries.lock() = entries;
    FailoverHandle { inner }
}

/// The failover API routes, with their state already applied.
pub fn router(handle: FailoverHandle) -> axum::Router {
    http::router(handle)
}

/// A file source's player; stops it when dropped (a `ChannelHandle` alone
/// does not), so aborting the failover task never leaves one running.
struct FilePlayer(caudal_channel::ChannelHandle);

impl Drop for FilePlayer {
    fn drop(&mut self) {
        self.0.stop();
    }
}

/// What the runner last saw of one source.
#[derive(Default)]
struct Probe {
    stream: Option<Arc<Stream>>,
    frames_in: u64,
    last_progress: Option<Instant>,
}

enum Wake {
    Tick,
    Event(Event),
    Command(Option<Command>),
}

struct Runner {
    cfg: Failover,
    registry: Arc<Registry>,
    buffer: BufferConfig,
    shared: Arc<Shared>,
    switches: broadcast::Sender<SwitchEvent>,
    sw: Switcher,
    probes: Vec<Probe>,
    healthy: Vec<bool>,
    files: Vec<Option<FilePlayer>>,
    out: Option<Publisher>,
    busy_logged: bool,
    sub: Option<Subscriber>,
    stitch: Stitcher,
}

async fn next_event(sub: &mut Option<Subscriber>) -> Event {
    match sub {
        Some(s) => s.recv().await,
        None => std::future::pending().await,
    }
}

fn unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

impl Runner {
    fn new(
        cfg: Failover,
        registry: Arc<Registry>,
        buffer: BufferConfig,
        shared: Arc<Shared>,
        switches: broadcast::Sender<SwitchEvent>,
    ) -> Self {
        let n = cfg.sources.len();
        Self {
            sw: Switcher::new(n, cfg.switch_back_after),
            probes: (0..n).map(|_| Probe::default()).collect(),
            healthy: vec![false; n],
            files: (0..n).map(|_| None).collect(),
            cfg,
            registry,
            buffer,
            shared,
            switches,
            out: None,
            busy_logged: false,
            sub: None,
            stitch: Stitcher::default(),
        }
    }

    async fn run(mut self, mut cmds: mpsc::Receiver<Command>) {
        if let Err(e) = self.cfg.validate() {
            tracing::error!(stream = %self.cfg.stream, %e, "failover: not starting");
            return;
        }
        tracing::info!(stream = %self.cfg.stream, sources = self.cfg.sources.len(), "failover: started");
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let wake = tokio::select! {
                _ = tick.tick() => Wake::Tick,
                ev = next_event(&mut self.sub) => Wake::Event(ev),
                cmd = cmds.recv() => Wake::Command(cmd),
            };
            match wake {
                Wake::Tick => self.tick(),
                Wake::Event(ev) => self.on_event(ev),
                Wake::Command(Some((source, reply))) => {
                    let res = self.manual(source);
                    self.publish_status();
                    let _ = reply.send(res);
                }
                // The handle is gone; keep running until aborted.
                Wake::Command(None) => std::future::pending::<()>().await,
            }
        }
    }

    /// The name the runner subscribes to for source `i`.
    fn source_stream(&self, i: usize) -> String {
        match &self.cfg.sources[i] {
            Source::Stream(n) => n.clone(),
            Source::File(_) => file_stream_name(&self.cfg.stream, i),
        }
    }

    fn tick(&mut self) {
        let now = Instant::now();
        for i in 0..self.cfg.sources.len() {
            let name = self.source_stream(i);
            let current = self.registry.get(&name).filter(|s| !s.is_ended());
            let p = &mut self.probes[i];
            match current {
                Some(s) => {
                    let frames = s.stats().frames_in;
                    let fresh = !p.stream.as_ref().is_some_and(|old| Arc::ptr_eq(old, &s));
                    if (fresh && frames > 0) || frames > p.frames_in {
                        p.last_progress = Some(now);
                    }
                    p.frames_in = frames;
                    p.stream = Some(s);
                }
                None => {
                    p.stream = None;
                    p.frames_in = 0;
                }
            }
            self.healthy[i] = match &self.cfg.sources[i] {
                // Still healthy for `switch_after` after the publisher
                // drops, so a quick republish is rejoined, not switched
                // away from.
                Source::Stream(_) => {
                    p.last_progress.is_some_and(|t| now.saturating_duration_since(t) < self.cfg.switch_after)
                }
                Source::File(path) => path.is_file(),
            };
        }
        self.sw.observe(now, &self.healthy);

        if let Some((i, reason)) = self.sw.decide(now, &self.healthy) {
            self.switch_to(i, reason);
        } else if let Some(a) = self.sw.active
            && self.sub.is_none()
        {
            // Waiting to join the active source: a file player starting, or
            // a publisher that dropped and came back within `switch_after`.
            if self.attach(a) {
                tracing::info!(stream = %self.cfg.stream, source = %self.cfg.sources[a].label(), "failover: source rejoined");
            }
        }
        self.publish_status();
    }

    /// Subscribes to source `i` if it is live; starts its player if it is
    /// a file. True when subscribed.
    fn attach(&mut self, i: usize) -> bool {
        if let Source::File(path) = &self.cfg.sources[i]
            && self.files[i].is_none()
        {
            let ch = caudal_channel::Channel {
                name: file_stream_name(&self.cfg.stream, i),
                items: vec![path.clone()],
                r#loop: true,
                shuffle: false,
            };
            let handle = caudal_channel::start(
                self.registry.clone(),
                // Internal slate: nothing reaches its skip route.
                caudal_channel::ChannelConfig { channels: vec![ch], buffer: self.buffer, trusted_proxies: Vec::new() },
            );
            self.files[i] = Some(FilePlayer(handle));
        }
        let Some(stream) = self.registry.get(&self.source_stream(i)).filter(|s| !s.is_ended()) else {
            return false;
        };
        self.sub = Some(stream.subscribe_internal(StartAt::LiveEdge));
        self.stitch.begin();
        true
    }

    fn switch_to(&mut self, i: usize, reason: Reason) {
        let from = self.sw.active.map(|a| self.cfg.sources[a].label());
        let to = self.cfg.sources[i].label();
        self.sw.active = Some(i);
        self.sub = None;
        // File players run only while on air.
        for (j, f) in self.files.iter_mut().enumerate() {
            if j != i {
                *f = None;
            }
        }
        self.attach(i);
        tracing::info!(stream = %self.cfg.stream, from = ?from, %to, reason = reason.as_str(), "failover: switched");
        let ev = SwitchEvent { stream: self.cfg.stream.clone(), from, to, reason: reason.as_str(), at_ms: unix_ms() };
        {
            let mut st = self.shared.status.lock();
            st.switches += 1;
            st.last_switch = Some(ev.clone());
            st.history.push(ev.clone());
            if st.history.len() > HISTORY {
                st.history.remove(0);
            }
        }
        let _ = self.switches.send(ev);
    }

    fn manual(&mut self, source: Option<String>) -> Result<(), SwitchError> {
        let Some(label) = source else {
            self.sw.preferred = None;
            tracing::info!(stream = %self.cfg.stream, "failover: back to automatic");
            return Ok(());
        };
        let i = self.cfg.sources.iter().position(|s| s.label() == label).ok_or(SwitchError::UnknownSource)?;
        if !self.healthy[i] {
            return Err(SwitchError::NotHealthy);
        }
        self.sw.preferred = Some(i);
        if self.sw.active != Some(i) {
            self.switch_to(i, Reason::Manual);
        }
        Ok(())
    }

    /// Publishes the public stream if it is not yet, with the tracks on air.
    fn ensure_output(&mut self) -> Option<&Publisher> {
        if self.out.is_none() {
            match self.registry.publish(&self.cfg.stream, self.buffer) {
                Ok(p) => {
                    tracing::info!(stream = %self.cfg.stream, "failover: on air");
                    self.busy_logged = false;
                    self.out = Some(p);
                }
                Err(PublishError::Busy(_)) => {
                    if !self.busy_logged {
                        tracing::warn!(stream = %self.cfg.stream, "failover: stream name already published; waiting");
                        self.busy_logged = true;
                    }
                    return None;
                }
                Err(PublishError::InvalidName) => return None,
            }
        }
        self.out.as_ref()
    }

    fn on_event(&mut self, ev: Event) {
        match ev {
            Event::TracksChanged => {
                let tracks = self.sub.as_ref().map(|s| s.tracks()).unwrap_or_default();
                self.stitch.set_tracks(tracks.clone());
                if let Some(out) = self.ensure_output()
                    && let Err(e) = out.set_tracks(tracks)
                {
                    tracing::debug!(stream = %self.cfg.stream, %e, "failover: set tracks failed");
                }
            }
            Event::Frame(f) => {
                let Some(frame) = self.stitch.stamp((*f).clone()) else { return };
                let tracks = self.sub.as_ref().map(|s| s.tracks());
                let fresh = self.out.is_none();
                if let Some(out) = self.ensure_output() {
                    if fresh && let Some(t) = tracks {
                        let _ = out.set_tracks(t);
                    }
                    if let Err(e) = out.push(frame) {
                        tracing::debug!(stream = %self.cfg.stream, %e, "failover: frame dropped");
                    }
                }
            }
            Event::Lagged { skipped } => {
                tracing::warn!(stream = %self.cfg.stream, skipped, "failover: fell behind the source; rejoining at a keyframe");
                self.stitch.begin();
            }
            Event::Cue(c) => {
                if let Some(at_us) = self.stitch.cue_time(c.at_us)
                    && let Some(out) = self.out.as_ref()
                {
                    let _ = out.push_cue(Cue { at_us, ..c });
                }
            }
            Event::End => {
                // The active publisher went away. It may come back within
                // `switch_after` (the next tick rejoins it); otherwise the
                // next tick fails over.
                self.sub = None;
            }
        }
    }

    fn publish_status(&self) {
        let now = Instant::now();
        let mut st = self.shared.status.lock();
        st.active = self.sw.active.map(|a| self.cfg.sources[a].label());
        st.preferred = self.sw.preferred.map(|p| self.cfg.sources[p].label());
        st.on_air = self.out.is_some();
        for (i, s) in st.sources.iter_mut().enumerate() {
            let p = &self.probes[i];
            s.live = p.stream.is_some();
            s.healthy = self.healthy[i];
            s.silent_ms = match self.cfg.sources[i] {
                Source::Stream(_) => p.last_progress.map(|t| now.saturating_duration_since(t).as_millis() as u64),
                Source::File(_) => None,
            };
            s.healthy_for_ms = self.sw.healthy_for(now, i).map(|d| d.as_millis() as u64);
        }
    }
}

#[cfg(test)]
mod tests;
