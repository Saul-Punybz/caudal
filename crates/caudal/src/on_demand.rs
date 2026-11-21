//! On-demand pulls: a pull that runs only while someone watches
//! (MistServer's on-demand inputs, `docs/research/MISTSERVER-REVERSE.md`
//! §1.4). See `docs/ON-DEMAND.md`.
//!
//! [`OnDemand`] is one more [`Demand`] source on the registry, next to the
//! cluster edge's: every output already calls [`Registry::get_or_demand`]
//! for a name nobody publishes, after the viewer passed `[auth]` and
//! `[[access.rules]]`. For a name with an `on_demand = true` pull entry (or,
//! failing that, a source URL from `[auth.source]`, MistServer's
//! `STREAM_SOURCE`), it starts the very pull an always-on entry runs (the
//! pull crates' own `start_pulls`, with a list of one), waits for the first
//! frame, and hands the stream back. One task per name watches the viewer
//! count (`Stream::stats().viewers`, which counts subscribers and HLS/MoQ/
//! OMT players) and stops the pull `idle_secs` after the last one left.
//!
//! - Viewers arriving together start one pull: the first inserts it in the
//!   map, the others wait on its state.
//! - No frame within [`FIRST_MEDIA`]: the pull is stopped, every waiting
//!   viewer gets the output's "not found", and the name is not tried again
//!   for [`BACKOFF`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use caudal_core::{BufferConfig, Demand, DemandFuture, Registry, Stream};
use parking_lot::Mutex;
use serde::Serialize;
use tokio::sync::watch;

/// Longest wait from starting a pull to its first frame.
pub const FIRST_MEDIA: Duration = Duration::from_secs(10);
/// After a pull produced nothing in time, requests for the name get "not
/// found" at once for this long.
pub const BACKOFF: Duration = Duration::from_secs(5);
/// Default `idle_secs`, as MistServer's.
pub const DEFAULT_IDLE: Duration = Duration::from_secs(30);
/// How often a running pull looks at its viewer count.
const TICK: Duration = Duration::from_millis(250);

/// What an on-demand pull pulls: the runtime type of one pull crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Srt(caudal_srt::SrtPull),
    Udp(caudal_udp::UdpPull),
    Http(caudal_http_pull::HttpPull),
    Rtmp(caudal_rtmp::RtmpPull),
    Hls(caudal_hls_pull::HlsPull),
    Rtsp(caudal_rtsp::RtspPull),
}

impl Source {
    /// The pull for a URL a `[auth.source]` service returned, by scheme:
    /// `srt://`, `udp://` / `rtp://`, `rtmp(s)://`, `rtsp(s)://`, and
    /// `http(s)://`: HLS when the path ends in `.m3u8`, MPEG-TS otherwise.
    pub fn from_url(stream: &str, url: &str) -> Result<Self, String> {
        let scheme = url.split_once("://").map(|(s, _)| s.to_ascii_lowercase()).unwrap_or_default();
        let (stream, url) = (stream.to_owned(), url.to_owned());
        Ok(match scheme.as_str() {
            "srt" => {
                caudal_srt::check_caller_url(&url)?;
                Self::Srt(caudal_srt::SrtPull { stream, url })
            }
            "udp" | "rtp" => {
                caudal_udp::parse_url(&url)?;
                Self::Udp(caudal_udp::UdpPull { stream, url, interface: None })
            }
            "rtmp" | "rtmps" => {
                caudal_rtmp::pull::parse_url(&url)?;
                Self::Rtmp(caudal_rtmp::RtmpPull { stream, url })
            }
            "rtsp" | "rtsps" => {
                let ok = url::Url::parse(&url).is_ok_and(|u| u.host_str().is_some_and(|h| !h.is_empty()));
                if !ok {
                    return Err(format!("{:?}: expected rtsp://host/path", redact(&url)));
                }
                Self::Rtsp(caudal_rtsp::RtspPull { stream, url })
            }
            "http" | "https" => {
                if caudal_hls_pull::is_playlist_url(&url) {
                    caudal_hls_pull::parse_url(&url)?;
                    Self::Hls(caudal_hls_pull::HlsPull { stream, url })
                } else {
                    caudal_http_pull::parse_url(&url)?;
                    Self::Http(caudal_http_pull::HttpPull { stream, url })
                }
            }
            _ => {
                return Err(format!(
                    "{:?}: not a kind of source Caudal pulls (srt, udp, rtp, http, https, rtmp, rtmps, rtsp, rtsps)",
                    redact(&url)
                ));
            }
        })
    }

    /// The config section's short name: `srt`, `udp`, `http`, `rtmp`,
    /// `hls`, `rtsp`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Srt(_) => "srt",
            Self::Udp(_) => "udp",
            Self::Http(_) => "http",
            Self::Rtmp(_) => "rtmp",
            Self::Hls(_) => "hls",
            Self::Rtsp(_) => "rtsp",
        }
    }

    pub fn url(&self) -> &str {
        match self {
            Self::Srt(p) => &p.url,
            Self::Udp(p) => &p.url,
            Self::Http(p) => &p.url,
            Self::Rtmp(p) => &p.url,
            Self::Hls(p) => &p.url,
            Self::Rtsp(p) => &p.url,
        }
    }

    fn start(&self, registry: &Arc<Registry>, buffer: BufferConfig) -> Running {
        let r = registry.clone();
        match self.clone() {
            Self::Srt(p) => Running::Srt(caudal_srt::start_pulls(r, buffer, vec![p])),
            Self::Udp(p) => Running::Udp(caudal_udp::start_pulls(r, buffer, vec![p])),
            Self::Http(p) => Running::Http(caudal_http_pull::start_pulls(r, buffer, vec![p])),
            Self::Rtmp(p) => Running::Rtmp(caudal_rtmp::start_pulls(r, buffer, vec![p])),
            Self::Hls(p) => Running::Hls(caudal_hls_pull::start_pulls(r, buffer, vec![p])),
            Self::Rtsp(p) => Running::Rtsp(caudal_rtsp::start_pulls(r, buffer, vec![p])),
        }
    }
}

/// A started pull: the pull crate's own handle, holding the one task.
enum Running {
    Srt(caudal_srt::PullHandle),
    Udp(caudal_udp::UdpPullHandle),
    Http(caudal_http_pull::HttpPullHandle),
    Rtmp(caudal_rtmp::RtmpPullHandle),
    Hls(caudal_hls_pull::HlsPullHandle),
    Rtsp(caudal_rtsp::PullHandle),
}

impl Running {
    /// Aborts the pull's task; dropping its publisher ends the stream.
    fn stop(self, registry: &Arc<Registry>, buffer: BufferConfig) {
        match self {
            Self::Srt(h) => h.reload(registry, Vec::new()),
            Self::Udp(h) => h.reload(registry, Vec::new()),
            Self::Http(h) => h.reload(registry, Vec::new()),
            Self::Rtmp(h) => h.reload(registry, Vec::new()),
            Self::Hls(h) => h.reload(registry, Vec::new()),
            Self::Rtsp(h) => h.reload(registry, buffer, Vec::new()),
        }
    }
}

/// One configured `on_demand = true` pull entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub source: Source,
    pub idle: Duration,
}

/// `[auth.source]`, installed.
#[derive(Clone)]
pub struct SourceHook {
    pub hook: Arc<caudal_auth::SourceHook>,
    pub idle: Duration,
}

#[derive(Clone)]
enum State {
    Starting,
    Live(Arc<Stream>),
    /// No first frame in time.
    Failed,
    /// Idle (or removed by a reload): the source is being stopped.
    Stopping,
    /// Out of the map; a new request starts a new pull.
    Gone,
}

impl State {
    fn as_str(&self) -> &'static str {
        match self {
            State::Starting => "starting",
            State::Live(_) => "live",
            State::Failed => "failed",
            State::Stopping => "stopping",
            State::Gone => "idle",
        }
    }
}

/// Where a running pull's source came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    Config,
    Hook,
}

impl Origin {
    fn as_str(self) -> &'static str {
        match self {
            Origin::Config => "config",
            Origin::Hook => "hook",
        }
    }
}

struct Pull {
    source: Source,
    origin: Origin,
    idle: Duration,
    state: watch::Sender<State>,
    /// Set by a reload that removed or changed the entry.
    cancel: watch::Sender<bool>,
}

#[derive(Default)]
struct Counters {
    starts: AtomicU64,
    failures: AtomicU64,
    idle_stops: AtomicU64,
}

/// The on-demand pulls of one server. Install with [`OnDemand::install`].
pub struct OnDemand {
    registry: Weak<Registry>,
    buffer: BufferConfig,
    entries: Mutex<HashMap<String, Entry>>,
    hook: Mutex<Option<SourceHook>>,
    pulls: Mutex<HashMap<String, Arc<Pull>>>,
    /// Name -> no new attempt before then.
    backoff: Mutex<HashMap<String, Instant>>,
    counters: Counters,
    /// `[auth.source]` counters, kept across reloads.
    pub hook_stats: Arc<caudal_auth::SourceHookStats>,
}

/// One row of `GET /api/v1/pulls`: the config entry as
/// `crate::config_edit::list` shows it, plus what runs.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PullRow {
    #[serde(flatten)]
    pub entry: crate::config_edit::EntryJson,
    pub on_demand: bool,
    /// On-demand: `idle`, `starting`, `live`, `stopping`, `failed`.
    /// Always-on: `live` or `connecting`.
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_secs: Option<u64>,
    pub viewers: usize,
}

struct Installed(Arc<OnDemand>);

impl Demand for Installed {
    fn demand<'a>(&'a self, name: &'a str) -> DemandFuture<'a> {
        Box::pin(self.0.demand(name))
    }
}

impl OnDemand {
    /// Creates the on-demand pulls and installs them as a demand source on
    /// `registry` (key `on-demand`, asked before a cluster edge installed
    /// later). Must be called inside a tokio runtime.
    pub fn install(registry: &Arc<Registry>, buffer: BufferConfig) -> Arc<Self> {
        let me = Arc::new(Self {
            registry: Arc::downgrade(registry),
            buffer,
            entries: Mutex::default(),
            hook: Mutex::default(),
            pulls: Mutex::default(),
            backoff: Mutex::default(),
            counters: Counters::default(),
            hook_stats: Arc::default(),
        });
        registry.set_demand("on-demand", Arc::new(Installed(me.clone())));
        me
    }

    /// Replaces the configured entries. A running pull whose entry was
    /// removed or changed stops now (ending its stream; its viewers'
    /// players reconnect and start the new one); the others keep running.
    pub fn configure(&self, entries: HashMap<String, Entry>) {
        let mut current = self.entries.lock();
        for (name, pull) in self.pulls.lock().iter() {
            if pull.origin == Origin::Config && entries.get(name).is_none_or(|e| e.source != pull.source) {
                pull.cancel.send_replace(true);
            }
        }
        self.backoff.lock().clear();
        *current = entries;
    }

    /// Installs or removes `[auth.source]`. Pulls it already started keep
    /// running until idle.
    pub fn set_hook(&self, hook: Option<SourceHook>) {
        *self.hook.lock() = hook;
        self.backoff.lock().clear();
    }

    async fn demand(self: &Arc<Self>, name: &str) -> Option<Arc<Stream>> {
        // A second round only after waiting out a pull that was stopping.
        for _ in 0..2 {
            let existing = self.pulls.lock().get(name).cloned();
            let pull = match existing {
                Some(p) => p,
                None => self.start(name).await?,
            };
            let mut rx = pull.state.subscribe();
            let state = rx.wait_for(|s| !matches!(s, State::Starting)).await.ok()?.clone();
            match state {
                State::Live(s) => return Some(s),
                State::Stopping => {
                    let _ = rx.wait_for(|s| matches!(s, State::Gone)).await;
                }
                State::Failed | State::Gone | State::Starting => return None,
            }
        }
        None
    }

    /// Starts (or joins one started meanwhile) the pull for `name`; `None`
    /// when nothing configures it and the hook has no source.
    async fn start(self: &Arc<Self>, name: &str) -> Option<Arc<Pull>> {
        if self.backoff.lock().get(name).is_some_and(|until| Instant::now() < *until) {
            return None;
        }
        let configured = self.entries.lock().get(name).cloned();
        let (entry, origin) = match configured {
            Some(e) => (e, Origin::Config),
            None => {
                let hook = self.hook.lock().clone()?;
                let url = hook.hook.source(name).await?;
                match Source::from_url(name, &url) {
                    Ok(source) => (Entry { source, idle: hook.idle }, Origin::Hook),
                    Err(e) => {
                        tracing::warn!(stream = name, error = %e, "[auth.source] answered a source Caudal cannot pull");
                        return None;
                    }
                }
            }
        };
        let registry = self.registry.upgrade()?;
        let mut pulls = self.pulls.lock();
        if let Some(p) = pulls.get(name) {
            return Some(p.clone());
        }
        let pull = Arc::new(Pull {
            source: entry.source,
            origin,
            idle: entry.idle,
            state: watch::channel(State::Starting).0,
            cancel: watch::channel(false).0,
        });
        pulls.insert(name.to_owned(), pull.clone());
        drop(pulls);
        self.counters.starts.fetch_add(1, Ordering::Relaxed);
        tracing::info!(
            stream = name, kind = pull.source.kind(), url = %redact(pull.source.url()), origin = origin.as_str(),
            "on-demand pull starting",
        );
        let running = pull.source.start(&registry, self.buffer);
        tokio::spawn(self.clone().run(registry, name.to_owned(), pull.clone(), running));
        Some(pull)
    }

    async fn run(self: Arc<Self>, registry: Arc<Registry>, name: String, pull: Arc<Pull>, running: Running) {
        let mut cancel = pull.cancel.subscribe();
        let deadline = Instant::now() + FIRST_MEDIA;
        let first = loop {
            if let Some(s) = registry.get(&name).filter(|s| !s.is_ended() && s.stats().frames_in > 0) {
                break Some(s);
            }
            if Instant::now() >= deadline || *cancel.borrow() {
                break None;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let Some(stream) = first else {
            tracing::warn!(stream = %name, within = ?FIRST_MEDIA, "on-demand pull: no media; not available");
            self.counters.failures.fetch_add(1, Ordering::Relaxed);
            self.backoff.lock().insert(name.clone(), Instant::now() + BACKOFF);
            pull.state.send_replace(State::Failed);
            self.finish(&registry, &name, &pull, running).await;
            return;
        };
        tracing::info!(stream = %name, "on-demand pull live");
        pull.state.send_replace(State::Live(stream.clone()));
        // The grace for the first viewer to show up in the counts, too.
        let mut last_viewer = Instant::now();
        let mut current = stream;
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                _ = cancel.changed() => {}
            }
            if *cancel.borrow() {
                tracing::info!(stream = %name, "on-demand pull: entry changed or removed; stopped");
                break;
            }
            // The pull crate republishes after its source reconnects: keep
            // handing out the stream that is live now.
            if let Some(s) = registry.get(&name).filter(|s| !Arc::ptr_eq(s, &current)) {
                current = s;
                pull.state.send_replace(State::Live(current.clone()));
            }
            if !current.is_ended() && current.stats().viewers > 0 {
                last_viewer = Instant::now();
            } else if last_viewer.elapsed() >= pull.idle {
                tracing::info!(stream = %name, idle = ?pull.idle, "on-demand pull: no viewers; stopped");
                self.counters.idle_stops.fetch_add(1, Ordering::Relaxed);
                break;
            }
        }
        pull.state.send_replace(State::Stopping);
        self.finish(&registry, &name, &pull, running).await;
    }

    /// Stops the source, waits (briefly) for its stream to leave the
    /// registry, then leaves the map: a request from then on starts anew.
    async fn finish(&self, registry: &Arc<Registry>, name: &str, pull: &Arc<Pull>, running: Running) {
        running.stop(registry, self.buffer);
        let deadline = Instant::now() + Duration::from_secs(2);
        while registry.get(name).is_some() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        {
            let mut pulls = self.pulls.lock();
            if pulls.get(name).is_some_and(|p| Arc::ptr_eq(p, pull)) {
                pulls.remove(name);
            }
        }
        pull.state.send_modify(|s| {
            if !matches!(s, State::Failed) {
                *s = State::Gone;
            }
        });
    }

    /// `GET /api/v1/pulls`: the configured entries (`rows`, from
    /// `config_edit::list`) with their state, then the pulls
    /// `[auth.source]` started (section `[auth.source]`).
    pub fn rows(&self, rows: Vec<crate::config_edit::EntryJson>) -> Vec<PullRow> {
        let registry = self.registry.upgrade();
        let live = |name: &str| registry.as_ref().and_then(|r| r.get(name)).filter(|s| !s.is_ended());
        let viewers = |name: &str| live(name).map_or(0, |s| s.stats().viewers);
        let pulls = self.pulls.lock().clone();
        let entries = self.entries.lock().clone();
        let mut out: Vec<PullRow> = rows
            .into_iter()
            .map(|entry| {
                let name = entry.stream.clone();
                match entries.get(&name) {
                    Some(e) => PullRow {
                        state: pulls
                            .get(&name)
                            .filter(|p| p.origin == Origin::Config)
                            .map_or(State::Gone.as_str(), |p| p.state.borrow().as_str()),
                        on_demand: true,
                        idle_secs: Some(e.idle.as_secs()),
                        viewers: viewers(&name),
                        entry,
                    },
                    None => PullRow {
                        state: if live(&name).is_some() { "live" } else { "connecting" },
                        on_demand: false,
                        idle_secs: None,
                        viewers: viewers(&name),
                        entry,
                    },
                }
            })
            .collect();
        let mut hooked: Vec<_> = pulls.iter().filter(|(_, p)| p.origin == Origin::Hook).collect();
        hooked.sort_by(|a, b| a.0.cmp(b.0));
        for (name, p) in hooked {
            out.push(PullRow {
                entry: crate::config_edit::EntryJson {
                    stream: name.clone(),
                    url: crate::config_edit::redact(p.source.url()),
                    section: "[auth.source]",
                },
                on_demand: true,
                state: p.state.borrow().as_str(),
                idle_secs: Some(p.idle.as_secs()),
                viewers: viewers(name),
            });
        }
        out
    }

    /// Appends `caudal_on_demand_*` and `caudal_source_hook_*` to `out`.
    pub fn render_metrics(&self, out: &mut String) {
        use std::fmt::Write as _;
        let pulls = self.pulls.lock().clone();
        let mut by_state = [("starting", 0u64), ("live", 0), ("stopping", 0)];
        for p in pulls.values() {
            let st = p.state.borrow().as_str();
            if let Some(e) = by_state.iter_mut().find(|(s, _)| *s == st) {
                e.1 += 1;
            }
        }
        let _ = writeln!(out, "# HELP caudal_on_demand_pulls On-demand pulls running now, by state.");
        let _ = writeln!(out, "# TYPE caudal_on_demand_pulls gauge");
        for (state, n) in by_state {
            let _ = writeln!(out, "caudal_on_demand_pulls{{state=\"{state}\"}} {n}");
        }
        let c = &self.counters;
        for (name, help, v) in [
            ("caudal_on_demand_starts_total", "On-demand pulls started by a viewer's request.", &c.starts),
            ("caudal_on_demand_failures_total", "On-demand pulls that produced no media in time.", &c.failures),
            (
                "caudal_on_demand_idle_stops_total",
                "On-demand pulls stopped after idle_secs without viewers.",
                &c.idle_stops,
            ),
        ] {
            let _ = writeln!(out, "# HELP {name} {help}");
            let _ = writeln!(out, "# TYPE {name} counter");
            let _ = writeln!(out, "{name} {}", v.load(Ordering::Relaxed));
        }
        let s = &self.hook_stats;
        let _ = writeln!(out, "# HELP caudal_source_hook_calls_total Calls to [auth.source], by outcome.");
        let _ = writeln!(out, "# TYPE caudal_source_hook_calls_total counter");
        for (outcome, n) in s.calls() {
            let _ = writeln!(out, "caudal_source_hook_calls_total{{outcome=\"{outcome}\"}} {n}");
        }
        let _ = writeln!(out, "# HELP caudal_source_hook_cached_total Requests [auth.source] answered from its cache.");
        let _ = writeln!(out, "# TYPE caudal_source_hook_cached_total counter");
        let _ = writeln!(out, "caudal_source_hook_cached_total {}", s.cached());
        let _ = writeln!(out, "# HELP caudal_source_hook_wait_seconds_total Time spent waiting on [auth.source].");
        let _ = writeln!(out, "# TYPE caudal_source_hook_wait_seconds_total counter");
        let _ = writeln!(out, "caudal_source_hook_wait_seconds_total {:.6}", s.wait_seconds());
    }
}

/// `url` without user info and query string (SRT passphrases, tokens), for
/// logs and the API.
pub fn redact(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut u) => {
            let _ = u.set_username("");
            let _ = u.set_password(None);
            u.set_query(None);
            u.set_fragment(None);
            u.to_string()
        }
        Err(_) => "<not a url>".to_owned(),
    }
}

#[cfg(test)]
mod tests;
