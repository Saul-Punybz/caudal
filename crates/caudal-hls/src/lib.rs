//! LL-HLS / CMAF output, classic HLS with MPEG-TS segments (`tsseg`, on
//! demand), the `/play/{name}` page, and continuous MPEG-TS over HTTP
//! (`/ts/{name}.ts`, [`http_ts`]). Entry point fixed by the orchestrator.
//!
//! One packager task per published stream turns frames into CMAF parts as
//! they arrive, so parts exist before the first viewer asks. HTTP handlers
//! only read the packager state; blocking playlist reloads and requests for
//! the preload-hinted part wait on a `watch` channel bumped by the packager.

mod dash;
pub mod http_ts;
mod packager;
mod tsseg;
mod vtt;

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::net::SocketAddr;
use std::sync::Weak;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use axum::Router;
use axum::extract::{ConnectInfo, Path, RawQuery, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bytes::Bytes;
use caudal_core::captions::{CaptionSource, TextTrack};
use caudal_core::{Event, Registry, StartAt, Stream, Subscriber};
use tokio::runtime::Handle;
use tokio::sync::{broadcast, watch};

use packager::{Lookup, Packager, VariantAttrs};
use tsseg::TsSegmenter;

#[derive(Debug, Clone, Copy)]
pub struct HlsConfig {
    /// Target partial-segment duration.
    pub part_ms: u32,
    /// Target full-segment duration.
    pub segment_ms: u32,
    /// Write SCTE-35 cues as `EXT-X-DATERANGE` (`SCTE35-OUT`/`IN`/`CMD`).
    pub cue_tags: bool,
    /// Also write the legacy `EXT-X-CUE-OUT` / `EXT-X-CUE-OUT-CONT` /
    /// `EXT-X-CUE-IN` tags many SSAI vendors still key on. Off by default:
    /// they are not in RFC 8216.
    pub cue_out_tags: bool,
    /// When a publisher drops and the same name is published again within
    /// this long, the new publish continues the same playlist (media
    /// sequence numbers keep counting, with an `EXT-X-DISCONTINUITY`), so
    /// players ride through an encoder reconnect. `ZERO` ends the playlist
    /// (`EXT-X-ENDLIST`) as soon as the publisher leaves.
    pub reconnect_grace: Duration,
    /// Also serve classic HLS with MPEG-TS segments (`/hls/{name}/ts.m3u8`,
    /// segments `t{seq}.ts`), for players that predate fMP4 and for MPEG-2
    /// video / MP2 audio. Started on a stream's first such request, stopped
    /// after [`TS_IDLE`] without one.
    pub ts: bool,
}

impl Default for HlsConfig {
    fn default() -> Self {
        Self {
            part_ms: 200,
            segment_ms: 2000,
            cue_tags: true,
            cue_out_tags: false,
            reconnect_grace: Duration::from_secs(10),
            ts: true,
        }
    }
}

/// A player that has not asked for a playlist in this long has left. LL-HLS
/// players reload every part (~200 ms); a player that fell back to plain HLS
/// reloads about once per target duration (2 s).
const VIEWER_IDLE: Duration = Duration::from_secs(10);

/// How long an ended stream keeps answering (with `#EXT-X-ENDLIST`), after
/// the reconnect grace (if any) has run out.
const LINGER: Duration = Duration::from_secs(30);

/// The TS rendition of a stream stops (its segments are dropped) after this
/// long without a request for its playlist or segments.
const TS_IDLE: Duration = Duration::from_secs(30);

const PLAY_HTML: &str = include_str!("../static/play.html");

/// Serves `/hls/{name}/master.m3u8` (players enter here), `/hls/{name}/index.m3u8`, `/hls/{name}/init.mp4`,
/// `/hls/{name}/{segment}.m4s` and `/play/{name}`; with [`HlsConfig::ts`],
/// also classic HLS at `/hls/{name}/ts.m3u8` + `/hls/{name}/t{seq}.ts`
/// (see `tsseg`); and live MPEG-DASH over the same segments at
/// `/hls/{name}/manifest.mpd` (see `dash`), with `/dash/time` for its
/// `UTCTiming`. Mounted at the root by
/// the server; paths are absolute.
///
/// Must be called inside a tokio runtime: it spawns the task that starts a
/// packager for every stream that gets published.
///
/// `trusted_proxies` is `[server] trusted_proxies` (see
/// `crates/caudal/src/config.rs`): reverse proxies allowed to set
/// `X-Forwarded-For` for the address `Registry::authorize` (and
/// `caudal-access`'s `[[access.rules]]`) judges a play request by. Empty:
/// every peer's own address is used as is.
pub fn router(registry: Arc<Registry>, cfg: HlsConfig, trusted_proxies: Vec<caudal_core::Cidr>) -> axum::Router {
    router_with_captions(registry, cfg, trusted_proxies, None)
}

/// [`router`], plus a WebVTT subtitle rendition for every stream
/// `captions` has a text track for: `#EXT-X-MEDIA:TYPE=SUBTITLES` in the
/// multivariant playlist and a subtitle playlist `/hls/{name}/subs.m3u8`
/// whose segments (`c{msn}.vtt`) and parts (`c{msn}.p{i}.vtt`) mirror the
/// media ones, each rendered once, when its media counterpart closes.
pub fn router_with_captions(
    registry: Arc<Registry>,
    cfg: HlsConfig,
    trusted_proxies: Vec<caudal_core::Cidr>,
    captions: Option<Arc<dyn CaptionSource>>,
) -> axum::Router {
    let cfg = HlsConfig {
        part_ms: cfg.part_ms.max(10),
        segment_ms: cfg.segment_ms.max(cfg.part_ms.max(10)),
        cue_tags: cfg.cue_tags,
        cue_out_tags: cfg.cue_out_tags,
        reconnect_grace: cfg.reconnect_grace,
        ts: cfg.ts,
    };
    let hls = Arc::new(Hls { registry: registry.clone(), cfg, trusted_proxies, captions, streams: Mutex::default() });
    match Handle::try_current() {
        Ok(rt) => {
            // Subscribe before listing, so a publish in between is seen at
            // least once (`start` ignores duplicates).
            let publishes = registry.subscribe_publishes();
            for stream in registry.list() {
                hls.start(&rt, stream);
            }
            rt.spawn(listen(hls.clone(), rt.clone(), publishes));
        }
        Err(_) => tracing::error!("caudal_hls::router called outside a tokio runtime; no packagers will run"),
    }
    Router::new()
        .route("/hls/{name}/{file}", get(hls_file))
        .route("/play/{name}", get(play))
        .route("/dash/time", get(dash_time))
        .route("/dash/{name}/{file}", get(dash_alias))
        .with_state(hls)
}

struct Hls {
    registry: Arc<Registry>,
    cfg: HlsConfig,
    trusted_proxies: Vec<caudal_core::Cidr>,
    captions: Option<Arc<dyn CaptionSource>>,
    streams: Mutex<HashMap<String, Arc<Entry>>>,
}

impl Hls {
    /// The text track of `name`'s family root, if it is captioned.
    fn text_track(&self, name: &str) -> Option<TextTrack> {
        self.captions.as_ref()?.track(family_root(name))
    }
}

/// One stream name's packager and the channel its waiters sleep on. Outlives
/// one publish when the name is republished within the reconnect grace.
struct Entry {
    /// The publish currently feeding the packager.
    stream: Mutex<Arc<Stream>>,
    /// A republish waiting to take over from the current one.
    handoff: Mutex<Handoff>,
    handoff_ready: tokio::sync::Notify,
    pkg: Mutex<Packager>,
    tick: watch::Sender<u64>,
    /// Players seen recently, keyed by a hash of address + user agent, each
    /// with its viewer session (closed when the player goes idle).
    viewers: Mutex<HashMap<u64, Viewer>>,
    /// WebVTT parts and segments, rendered when their media counterparts
    /// close, under the packager lock (so the subtitle playlist, rendered
    /// under the same lock, never lists one that does not exist yet, and a
    /// file's text never changes once listed).
    subs: Mutex<VecDeque<SubSeg>>,
    /// The classic-HLS (TS segments) rendition, fed by its own task while
    /// players ask for it.
    ts: Mutex<TsState>,
    ts_tick: watch::Sender<u64>,
}

struct TsState {
    /// A task is feeding `seg`.
    running: bool,
    last_request: Instant,
    seg: TsSegmenter,
}

/// One LL-HLS player: plain HTTP requests, so its session is inferred from
/// address + user agent and ends after [`VIEWER_IDLE`] without a request.
struct Viewer {
    last: Instant,
    session: caudal_core::Session,
}

/// The WebVTT files of one media segment.
struct SubSeg {
    msn: u64,
    parts: Vec<Bytes>,
    full: Option<Bytes>,
}

#[derive(Default)]
struct Handoff {
    next: Option<Subscriber>,
    /// The grace ran out: the playlist ended and takes no successor.
    closed: bool,
}

impl Entry {
    fn stream(&self) -> Arc<Stream> {
        self.stream.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn handoff(&self) -> MutexGuard<'_, Handoff> {
        self.handoff.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The next publish of this name, if one arrives before `deadline`.
    /// Past it, closes the entry to successors.
    async fn successor(&self, deadline: tokio::time::Instant) -> Option<Subscriber> {
        loop {
            {
                let mut h = self.handoff();
                if let Some(next) = h.next.take() {
                    return Some(next);
                }
                if tokio::time::Instant::now() >= deadline {
                    h.closed = true;
                    return None;
                }
            }
            let _ = tokio::time::timeout_at(deadline, self.handoff_ready.notified()).await;
        }
    }

    fn ts(&self) -> MutexGuard<'_, TsState> {
        self.ts.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Like [`Entry::wait`], on the TS segmenter.
    async fn wait_ts<T>(&self, timeout: Duration, f: impl Fn(&TsSegmenter) -> Option<T>) -> Option<T> {
        let mut rx = self.ts_tick.subscribe();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            rx.borrow_and_update();
            if let Some(v) = f(&self.ts().seg) {
                return Some(v);
            }
            match tokio::time::timeout_at(deadline, rx.changed()).await {
                Ok(Ok(())) => {}
                _ => return None,
            }
        }
    }

    /// Stops the TS rendition if nobody asked for it in [`TS_IDLE`].
    fn ts_stop_if_idle(&self) -> bool {
        let mut ts = self.ts();
        if ts.last_request.elapsed() < TS_IDLE {
            return false;
        }
        ts.seg.idle();
        ts.running = false;
        drop(ts);
        self.ts_tick.send_modify(|v| *v = v.wrapping_add(1));
        true
    }

    fn pkg(&self) -> MutexGuard<'_, Packager> {
        self.pkg.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Waits until `f` returns `Some`, or `timeout` passes.
    async fn wait<T>(&self, timeout: Duration, f: impl Fn(&Packager) -> Option<T>) -> Option<T> {
        let mut rx = self.tick.subscribe();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            rx.borrow_and_update();
            if let Some(v) = f(&self.pkg()) {
                return Some(v);
            }
            match tokio::time::timeout_at(deadline, rx.changed()).await {
                Ok(Ok(())) => {}
                _ => return None,
            }
        }
    }

    /// A playlist request from player `key`: opens its session the first
    /// time. `Err` when an operator kicked it; it stays refused until it
    /// goes quiet for [`VIEWER_IDLE`].
    fn seen(&self, registry: &Registry, name: &str, key: u64, ip: Option<std::net::IpAddr>) -> Result<(), ()> {
        let mut v = self.viewers.lock().unwrap_or_else(|e| e.into_inner());
        let viewer = v
            .entry(key)
            .or_insert_with(|| Viewer { last: Instant::now(), session: registry.open_session(name, "hls", ip) });
        viewer.last = Instant::now();
        if viewer.session.is_kicked() { Err(()) } else { Ok(()) }
    }

    /// Any other request from `key` (parts, segments, init): refused when
    /// it was kicked, never opens a session.
    fn kicked(&self, key: u64) -> bool {
        let mut v = self.viewers.lock().unwrap_or_else(|e| e.into_inner());
        match v.get_mut(&key) {
            Some(viewer) if viewer.session.is_kicked() => {
                viewer.last = Instant::now();
                true
            }
            _ => false,
        }
    }

    fn add_bytes(&self, key: u64, n: u64) {
        if let Some(viewer) = self.viewers.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
            viewer.session.add_bytes(n as usize);
        }
    }

    /// Drops players idle past `VIEWER_IDLE` (closing their sessions) and
    /// reports the rest; kicked players are not counted.
    fn count_viewers(&self) {
        let n = {
            let mut v = self.viewers.lock().unwrap_or_else(|e| e.into_inner());
            v.retain(|_, viewer| viewer.last.elapsed() < VIEWER_IDLE);
            v.values().filter(|viewer| !viewer.session.is_kicked()).count()
        };
        self.stream().set_output_viewers("hls", n);
    }

    fn block_timeout(&self) -> Duration {
        Duration::from_secs(3 * self.pkg().target_duration())
    }
}

impl Hls {
    fn streams(&self) -> MutexGuard<'_, HashMap<String, Arc<Entry>>> {
        self.streams.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn get(&self, name: &str) -> Option<Arc<Entry>> {
        self.streams().get(name).cloned()
    }

    fn start(self: &Arc<Self>, rt: &Handle, stream: Arc<Stream>) {
        let entry = {
            let mut map = self.streams();
            if let Some(e) = map.get(stream.name()) {
                if Arc::ptr_eq(&e.stream(), &stream) {
                    return;
                }
                // The same name again while the old playlist is live or in
                // its reconnect grace: hand the new publish to its runner,
                // which switches over once the old one has drained.
                let mut h = e.handoff();
                if !self.cfg.reconnect_grace.is_zero() && !h.closed {
                    if h.next.as_ref().is_some_and(|s| Arc::ptr_eq(s.stream(), &stream)) {
                        return;
                    }
                    h.next = Some(stream.subscribe_internal(StartAt::LiveEdge));
                    drop(h);
                    e.handoff_ready.notify_one();
                    tracing::info!(stream = %stream.name(), "republished within the reconnect grace; ll-hls playlist continues");
                    return;
                }
            }
            let entry = Arc::new(Entry {
                stream: Mutex::new(stream.clone()),
                handoff: Mutex::default(),
                handoff_ready: tokio::sync::Notify::new(),
                pkg: Mutex::new(Packager::new(self.cfg)),
                tick: watch::channel(0).0,
                viewers: Mutex::default(),
                subs: Mutex::default(),
                ts: Mutex::new(TsState {
                    running: false,
                    last_request: Instant::now(),
                    seg: TsSegmenter::new(self.cfg.segment_ms),
                }),
                ts_tick: watch::channel(0).0,
            });
            map.insert(stream.name().to_owned(), entry.clone());
            entry
        };
        // Subscribe now, not when the task first runs, so no frame pushed
        // in between is missed.
        // The packager is not a viewer; players are counted per request.
        let sub = stream.subscribe_internal(StartAt::LiveEdge);
        tracing::debug!(stream = %stream.name(), "ll-hls packager started");
        rt.spawn(count_viewers(Arc::downgrade(&entry)));
        rt.spawn(run(self.clone(), entry, sub));
    }
}

async fn listen(hls: Arc<Hls>, rt: Handle, mut publishes: broadcast::Receiver<Arc<Stream>>) {
    loop {
        match publishes.recv().await {
            Ok(stream) => hls.start(&rt, stream),
            Err(broadcast::error::RecvError::Lagged(_)) => {
                for stream in hls.registry.list() {
                    hls.start(&rt, stream);
                }
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Once a second, until the entry is dropped.
async fn count_viewers(entry: Weak<Entry>) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tick.tick().await;
        match entry.upgrade() {
            Some(e) => e.count_viewers(),
            None => return,
        }
    }
}

/// The address `Registry::authorize` should judge this request by: the TCP
/// peer, or (only if it is itself a trusted proxy) the right-most untrusted
/// `X-Forwarded-For` hop. `None` when the server wasn't started with
/// connect info (never true in `crate::main::run`; only possible in a test
/// harness that skips `into_make_service_with_connect_info`).
fn client_ip(req: &Request, trusted_proxies: &[caudal_core::Cidr]) -> Option<std::net::IpAddr> {
    let peer = req.extensions().get::<ConnectInfo<SocketAddr>>()?.0.ip();
    let xff = req.headers().get("x-forwarded-for").and_then(|v| v.to_str().ok());
    Some(caudal_core::net::resolve_forwarded(peer, xff, trusted_proxies))
}

/// A stable key for one player: its address (the resolved client address,
/// so players behind one trusted proxy stay apart) plus its user agent.
fn viewer_key(req: &Request, ip: Option<std::net::IpAddr>) -> u64 {
    let mut h = DefaultHasher::new();
    ip.hash(&mut h);
    req.headers().get(header::USER_AGENT).map(|v| v.as_bytes()).hash(&mut h);
    h.finish()
}

/// The packager task for one stream name: one publish after another while
/// each republish lands within the reconnect grace.
async fn run(hls: Arc<Hls>, entry: Arc<Entry>, mut sub: Subscriber) {
    let grace = hls.cfg.reconnect_grace;
    loop {
        pump(&hls, &entry, &mut sub, !grace.is_zero()).await;
        drop(sub);
        let next = if grace.is_zero() { None } else { entry.successor(tokio::time::Instant::now() + grace).await };
        match next {
            Some(next) => {
                tracing::info!(stream = %next.stream().name(), "ll-hls packager resumed after a publisher reconnect");
                *entry.stream.lock().unwrap_or_else(|e| e.into_inner()) = next.stream().clone();
                {
                    let mut pkg = entry.pkg();
                    pkg.resume();
                    render_subs(&hls, &entry, &pkg);
                }
                entry.tick.send_modify(|v| *v = v.wrapping_add(1));
                sub = next;
            }
            None => {
                {
                    let mut pkg = entry.pkg();
                    pkg.end();
                    render_subs(&hls, &entry, &pkg);
                }
                entry.tick.send_modify(|v| *v = v.wrapping_add(1));
                break;
            }
        }
    }
    tokio::time::sleep(LINGER).await;
    let name = entry.stream().name().to_owned();
    let mut map = hls.streams();
    if map.get(&name).is_some_and(|e| Arc::ptr_eq(e, &entry)) {
        map.remove(&name);
    }
}

/// Feeds one publish into the packager until it ends. With `keep_open`, the
/// end only suspends the playlist; the caller decides whether it ends.
async fn pump(hls: &Hls, entry: &Entry, sub: &mut Subscriber, keep_open: bool) {
    loop {
        let ev = sub.recv().await;
        let changed = {
            let mut pkg = entry.pkg();
            let before = (pkg.last_part(), pkg.segments.len(), pkg.init.is_some());
            match &ev {
                Event::TracksChanged => {
                    pkg.set_tracks(&sub.tracks());
                }
                Event::Frame(f) => pkg.push(f, SystemTime::now()),
                Event::Lagged { skipped } => {
                    tracing::warn!(stream = %sub.stream().name(), skipped, "ll-hls packager lagged");
                    pkg.lagged();
                }
                Event::Cue(cue) => pkg.push_cue(cue),
                Event::End if keep_open => pkg.suspend(),
                Event::End => pkg.end(),
            }
            let changed =
                pkg.ended || pkg.suspended || before != (pkg.last_part(), pkg.segments.len(), pkg.init.is_some());
            if changed {
                render_subs(hls, entry, &pkg);
            }
            changed
        };
        if changed {
            entry.tick.send_modify(|v| *v = v.wrapping_add(1));
        }
        if ev == Event::End {
            return;
        }
    }
}

/// Renders the WebVTT file of every media part and segment that closed
/// since the last call, and forgets those that left the window. Called with
/// the packager locked. A no-op for a stream without captions.
fn render_subs(hls: &Hls, entry: &Entry, pkg: &Packager) {
    let Some(captions) = hls.captions.as_ref() else { return };
    let name = entry.stream().name().to_owned();
    if captions.track(&name).is_none() {
        return;
    }
    let mut subs = entry.subs.lock().unwrap_or_else(|e| e.into_inner());
    let first = pkg.segments.front().map_or(0, |s| s.msn);
    while subs.front().is_some_and(|s| s.msn < first) {
        subs.pop_front();
    }
    for (msn, part, start, end) in pkg.spans() {
        let seg = match subs.iter().position(|s| s.msn == msn) {
            Some(i) => &mut subs[i],
            None => {
                subs.push_back(SubSeg { msn, parts: Vec::new(), full: None });
                subs.back_mut().expect("just pushed")
            }
        };
        let done = match part {
            Some(i) => i < seg.parts.len(),
            None => seg.full.is_some(),
        };
        if done {
            continue;
        }
        let body = Bytes::from(vtt::segment(&captions.cues(&name, start, end), start, end));
        match part {
            Some(_) => seg.parts.push(body),
            None => seg.full = Some(body),
        }
    }
}

fn respond(
    status: StatusCode,
    content_type: &'static str,
    cache: &'static str,
    body: impl Into<axum::body::Body>,
) -> Response {
    let mut r = (status, body.into()).into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    r
}

fn error(status: StatusCode) -> Response {
    respond(status, "text/plain; charset=utf-8", "no-cache", status.canonical_reason().unwrap_or(""))
}

const PLAYLIST: &str = "application/vnd.apple.mpegurl";

async fn hls_file(
    State(hls): State<Arc<Hls>>,
    Path((name, file)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    req: Request,
) -> Response {
    let query = query.as_deref().unwrap_or("");
    let token = request_token(query, &req);
    let ip = client_ip(&req, &hls.trusted_proxies);
    if token.is_some_and(|t| !token_is_url_safe(t)) {
        return if hls.get(&name).is_some() { error(StatusCode::FORBIDDEN) } else { error(StatusCode::NOT_FOUND) };
    }
    // The rewrite hook (if any) and the gate, on the final name. Refusals
    // for a name nobody serves stay a plain 404, as before clustering.
    let requested = name;
    let name = match hls.registry.resolve(caudal_core::Access::Play, "hls", &requested, token, ip).await {
        Ok(n) => n,
        Err(_) if hls.get(&requested).is_none() => return error(StatusCode::NOT_FOUND),
        Err(caudal_core::Denied::Missing) => {
            let mut r = error(StatusCode::UNAUTHORIZED);
            r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            return r;
        }
        Err(caudal_core::Denied::Refused(_)) => return error(StatusCode::FORBIDDEN),
    };
    let entry = match hls.get(&name) {
        Some(e) => e,
        None => match demand(&hls, &name).await {
            Some(e) => e,
            None => return error(StatusCode::NOT_FOUND),
        },
    };
    let key = viewer_key(&req, ip);
    let manifest = file.ends_with(".m3u8") || file == "manifest.mpd";
    let refused = if manifest { entry.seen(&hls.registry, &name, key, ip).is_err() } else { entry.kicked(key) };
    if refused {
        return error(StatusCode::FORBIDDEN);
    }
    let resp = if file == "manifest.mpd" {
        let time_url = {
            let header = |h: &str| req.headers().get(h).and_then(|v| v.to_str().ok());
            dash::time_url(header("host"), header("x-forwarded-proto"))
        };
        dash_manifest(&hls, &name, &entry, token, time_url).await
    } else {
        let apple = loads_subtitle_parts(&req);
        serve_file(&hls, &name, &requested, &entry, &file, query, token, apple).await
    };
    if let Some(n) = axum::body::HttpBody::size_hint(resp.body()).exact() {
        entry.add_bytes(key, n);
    }
    resp
}

#[allow(clippy::too_many_arguments)]
async fn serve_file(
    hls: &Arc<Hls>,
    name: &str,
    // The name in the URL, before the rewrite hook: the master's URI for
    // `name` itself keeps it, so a routed viewer stays on its alias.
    requested: &str,
    entry: &Arc<Entry>,
    file: &str,
    query: &str,
    token: Option<&str>,
    apple: bool,
) -> Response {
    if hls.cfg.ts {
        if file == "ts.m3u8" {
            return ts_playlist(entry, token).await;
        }
        if let Some(seq) = parse_ts_name(file) {
            return ts_segment(entry, seq).await;
        }
    }
    let entry = entry.as_ref();
    match file {
        "index.m3u8" => playlist(hls, name, entry, query, token).await,
        "master.m3u8" => master(hls, entry, name, requested, token).await,
        "subs.m3u8" if hls.text_track(name).is_some() => {
            let flavor = Flavor::Subtitles { parts: apple };
            playlist_of(hls, name, entry, query, token, flavor).await
        }
        other if hls.text_track(name).is_some() && parse_vtt_name(other).is_some() => match parse_vtt_name(other) {
            Some((msn, part)) => subtitle(entry, msn, part).await,
            None => error(StatusCode::NOT_FOUND),
        },
        "init.mp4" => {
            // The first init segment while it is still listed; otherwise
            // the current one.
            let wait = entry.block_timeout();
            match entry.wait(wait, |p| p.init_for(0).or_else(|| p.init.clone())).await {
                Some(init) => respond(StatusCode::OK, "video/mp4", "no-cache", init),
                None => error(StatusCode::NOT_FOUND),
            }
        }
        other if parse_init_name(other).is_some() => {
            match parse_init_name(other).and_then(|g| entry.pkg().init_for(g)) {
                Some(init) => respond(StatusCode::OK, "video/mp4", "no-cache", init),
                None => error(StatusCode::NOT_FOUND),
            }
        }
        other if dash::parse_name(other).is_some() => match dash::parse_name(other) {
            Some(f) => dash_file(entry, f).await,
            None => error(StatusCode::NOT_FOUND),
        },
        other => match parse_media_name(other) {
            Some((msn, part)) => media(entry, msn, part).await,
            None => error(StatusCode::NOT_FOUND),
        },
    }
}

/// `GET /hls/{name}/manifest.mpd`. Like [`master`], a family root (no `+`)
/// brings in every live `{name}+*` rendition, and a root nobody can play
/// (no fMP4 segments of its own) gives way to its renditions.
async fn dash_manifest(
    hls: &Arc<Hls>,
    name: &str,
    entry: &Entry,
    token: Option<&str>,
    time_url: Option<String>,
) -> Response {
    let siblings = || -> Vec<(String, dash::Snap)> {
        if name.contains('+') {
            return Vec::new();
        }
        let prefix = format!("{name}+");
        let mut entries: Vec<(String, Arc<Entry>)> =
            hls.streams().iter().filter(|(n, _)| n.starts_with(&prefix)).map(|(n, e)| (n.clone(), e.clone())).collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries.into_iter().filter_map(|(n, e)| dash::Snap::of(&e.pkg()).map(|s| (n, s))).collect()
    };
    let (root_now, root_ready) = {
        let p = entry.pkg();
        (dash::Snap::of(&p), p.ready())
    };
    let mut family = siblings();
    let root = match root_now {
        Some(s) => Some(s),
        None if !root_ready && !family.is_empty() => None,
        None => entry.wait(entry.block_timeout(), dash::Snap::of).await,
    };
    let (lead_name, lead) = match root {
        Some(s) => (name.to_owned(), s),
        None => {
            if family.is_empty() {
                family = siblings();
            }
            if family.is_empty() {
                return error(if entry.pkg().ended { StatusCode::NOT_FOUND } else { StatusCode::SERVICE_UNAVAILABLE });
            }
            family.remove(0)
        }
    };
    let ctx = dash::Ctx { token, time_url, now: SystemTime::now() };
    let body = dash::render((&lead_name, &lead), &family, &ctx);
    respond(StatusCode::OK, dash::CONTENT_TYPE, "no-cache", body)
}

/// A one-track init segment or media segment named by a DASH manifest.
async fn dash_file(entry: &Entry, file: dash::File) -> Response {
    let (video, found) = match file {
        dash::File::Init { video, generation } => (video, dash::init(&entry.pkg(), video, generation)),
        dash::File::Media { video, msn } => {
            let wait = entry.block_timeout();
            let seg = entry
                .wait(wait, |p| match p.lookup(msn, None) {
                    Lookup::Found(b) => Some(dash::track_id(p, msn, video).map(|id| (b, id))),
                    Lookup::Gone => Some(None),
                    Lookup::Pending => None,
                })
                .await
                .flatten();
            let cut = seg.and_then(|(b, id)| caudal_cmaf::fmp4::extract_track(&b, id)).filter(|b| !b.is_empty());
            (video, cut)
        }
    };
    let ct = if video { "video/mp4" } else { "audio/mp4" };
    match (file, found) {
        (dash::File::Init { .. }, Some(b)) => respond(StatusCode::OK, ct, "no-cache", b),
        (dash::File::Media { .. }, Some(b)) => respond(StatusCode::OK, ct, "max-age=60", b),
        (_, None) => error(StatusCode::NOT_FOUND),
    }
}

/// `GET /dash/{name}/index.mpd` (MistServer's DASH URL) and the files its
/// manifest names: the same as under `/hls/{name}/`, DASH files only.
async fn dash_alias(
    State(hls): State<Arc<Hls>>,
    Path((name, file)): Path<(String, String)>,
    query: RawQuery,
    req: Request,
) -> Response {
    let file = match file.as_str() {
        "index.mpd" | "manifest.mpd" => "manifest.mpd".to_owned(),
        f if dash::parse_name(f).is_some() => file,
        _ => return error(StatusCode::NOT_FOUND),
    };
    hls_file(State(hls), Path((name, file)), query, req).await
}

/// `GET /dash/time`: the server's clock for MPD `UTCTiming`
/// (`urn:mpeg:dash:utc:http-iso:2014`), so players compute the live edge on
/// the same clock that dated the segments.
async fn dash_time() -> Response {
    respond(StatusCode::OK, "text/plain; charset=utf-8", "no-store", packager::rfc3339(SystemTime::now()))
}

/// `t{seq}.ts` → `seq`: a segment of the TS rendition.
fn parse_ts_name(file: &str) -> Option<u64> {
    let n = file.strip_prefix('t')?.strip_suffix(".ts")?;
    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    n.parse().ok()
}

/// A request for the TS rendition: starts its task if it is not running
/// (and the stream has not ended), and keeps it alive.
fn ts_touch(entry: &Arc<Entry>) {
    let mut ts = entry.ts();
    ts.last_request = Instant::now();
    if ts.running {
        return;
    }
    if entry.pkg().ended {
        // Too late to start: whatever was cut before stays, closed.
        ts.seg.end();
        return;
    }
    ts.running = true;
    tracing::debug!(stream = %entry.stream().name(), "ts-hls segmenter started");
    tokio::spawn(ts_run(entry.clone()));
}

/// `GET /hls/{name}/ts.m3u8`: waits for the first segment when the
/// rendition has just started.
async fn ts_playlist(entry: &Arc<Entry>, token: Option<&str>) -> Response {
    ts_touch(entry);
    let wait = Duration::from_secs(3 * entry.ts().seg.target_duration());
    // `Some(None)`: the stream ended before a single segment was cut.
    let body = entry
        .wait_ts(wait, |s| match (s.segments.is_empty(), s.ended) {
            (false, _) => Some(Some(s.playlist())),
            (true, true) => Some(None),
            (true, false) => None,
        })
        .await;
    match body {
        Some(Some(b)) => respond(StatusCode::OK, PLAYLIST, "no-cache", with_token(&b, token)),
        Some(None) => error(StatusCode::NOT_FOUND),
        None => error(StatusCode::SERVICE_UNAVAILABLE),
    }
}

/// `GET /hls/{name}/t{seq}.ts`. The segment being cut right now is waited
/// for; anything else is there or gone.
async fn ts_segment(entry: &Arc<Entry>, seq: u64) -> Response {
    ts_touch(entry);
    let wait = Duration::from_secs(3 * entry.ts().seg.target_duration());
    let found = entry
        .wait_ts(wait, |s| match s.segment(seq) {
            Some(b) => Some(Some(b)),
            None if seq == s.next_seq() && !s.ended => None,
            None => Some(None),
        })
        .await
        .flatten();
    match found {
        Some(b) => respond(StatusCode::OK, "video/mp2t", "max-age=60", b),
        None => error(StatusCode::NOT_FOUND),
    }
}

/// The TS rendition's task: subscribes to the publish feeding `entry`
/// (from its latest keyframe, so the first segment is ready about one
/// segment later), follows a republish within the reconnect grace, and
/// stops when the stream ends or nobody has asked for the rendition in
/// [`TS_IDLE`].
async fn ts_run(entry: Arc<Entry>) {
    let mut stream = entry.stream();
    let mut ticks = entry.tick.subscribe();
    let mut idle = tokio::time::interval(Duration::from_secs(1));
    loop {
        let mut sub = stream.subscribe_internal(StartAt::LiveEdge);
        loop {
            tokio::select! {
                ev = sub.recv() => {
                    let changed = {
                        let mut ts = entry.ts();
                        let before = (ts.seg.segments.len(), ts.seg.next_seq());
                        match &ev {
                            Event::TracksChanged => ts.seg.set_tracks(&sub.tracks()),
                            Event::Frame(f) => ts.seg.push(f),
                            Event::Lagged { .. } => ts.seg.lagged(),
                            Event::Cue(cue) => ts.seg.push_cue(cue),
                            Event::End => ts.seg.suspend(),
                        }
                        before != (ts.seg.segments.len(), ts.seg.next_seq())
                    };
                    if changed {
                        entry.ts_tick.send_modify(|v| *v = v.wrapping_add(1));
                    }
                    if ev == Event::End {
                        break;
                    }
                }
                _ = idle.tick() => if entry.ts_stop_if_idle() { return },
            }
        }
        // The publish is gone: a republish continues the playlist, the end
        // of the LL-HLS one ends this one too.
        loop {
            if entry.pkg().ended {
                {
                    let mut ts = entry.ts();
                    ts.seg.end();
                    ts.running = false;
                }
                entry.ts_tick.send_modify(|v| *v = v.wrapping_add(1));
                return;
            }
            let now = entry.stream();
            if !Arc::ptr_eq(&now, &stream) {
                stream = now;
                break;
            }
            tokio::select! {
                _ = ticks.changed() => {}
                _ = idle.tick() => if entry.ts_stop_if_idle() { return },
            }
        }
    }
}

/// `c{msn}.vtt` → `(msn, None)`, `c{msn}.p{part}.vtt` → `(msn, Some(part))`:
/// the WebVTT twins of `s{msn}.m4s` / `s{msn}.p{part}.m4s`.
fn parse_vtt_name(file: &str) -> Option<(u64, Option<usize>)> {
    let stem = file.strip_prefix('c')?.strip_suffix(".vtt")?;
    parse_media_name(&format!("s{stem}.m4s"))
}

/// Whether to list parts in the subtitle playlist for this client: yes for
/// Apple's own media stack (AVFoundation, `AppleCoreMedia` user agent:
/// Safari's native player, iOS/tvOS apps, and Apple's validator), which
/// Apple's LL-HLS rules are written for and which require them on every
/// rendition; no for everyone else, because hls.js 1.7 stalls loading
/// subtitle parts (it loads the first ones, then never finishes the
/// fragment: browser check, 19 Sep 2026) and plays whole WebVTT segments
/// fine. The segments and cue times are the same either way.
fn loads_subtitle_parts(req: &Request) -> bool {
    req.headers().get(header::USER_AGENT).and_then(|v| v.to_str().ok()).is_some_and(|ua| ua.contains("AppleCoreMedia"))
}

/// A request for a name with no packager: on a cluster edge, an
/// authorized viewer's first request starts the pull
/// ([`Registry::get_or_demand`]) and waits for its packager. Only called
/// after `Registry::resolve` allowed the request.
async fn demand(hls: &Arc<Hls>, name: &str) -> Option<Arc<Entry>> {
    hls.registry.get_or_demand(name).await?;
    // The packager starts from the publish event, on another task.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(e) = hls.get(name) {
            return Some(e);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// `init{gen}.m4s` (a later init segment, `gen` ≥ 1) → `gen`.
fn parse_init_name(file: &str) -> Option<u32> {
    let g = file.strip_prefix("init")?.strip_suffix(".mp4")?;
    if g.is_empty() || !g.bytes().all(|b| b.is_ascii_digit()) || g.starts_with('0') {
        return None;
    }
    g.parse().ok()
}

/// `s{msn}.m4s` → `(msn, None)`, `s{msn}.p{part}.m4s` → `(msn, Some(part))`.
fn parse_media_name(file: &str) -> Option<(u64, Option<usize>)> {
    let stem = file.strip_prefix('s')?.strip_suffix(".m4s")?;
    fn number<T: std::str::FromStr>(s: &str) -> Option<T> {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        s.parse().ok()
    }
    match stem.split_once(".p") {
        Some((m, p)) => Some((number(m)?, Some(number(p)?))),
        None => Some((number(stem)?, None)),
    }
}

async fn media(entry: &Entry, msn: u64, part: Option<usize>) -> Response {
    let wait = entry.block_timeout();
    let found = entry
        .wait(wait, |p| match p.lookup(msn, part) {
            Lookup::Found(b) => Some(Some(b)),
            Lookup::Gone => Some(None),
            Lookup::Pending => None,
        })
        .await
        .flatten();
    match found {
        Some(b) => respond(StatusCode::OK, "video/iso.segment", "max-age=60", b),
        None => error(StatusCode::NOT_FOUND),
    }
}

/// A subtitle part or segment. Like [`media`], a request for the part the
/// preload hint names waits for it.
async fn subtitle(entry: &Entry, msn: u64, part: Option<usize>) -> Response {
    let wait = entry.block_timeout();
    let found = entry
        .wait(wait, |p| match p.lookup(msn, part) {
            // The media part or segment exists, so its WebVTT twin was
            // rendered with it (same packager lock).
            Lookup::Found(_) => {
                let subs = entry.subs.lock().unwrap_or_else(|e| e.into_inner());
                let seg = subs.iter().find(|s| s.msn == msn);
                Some(seg.and_then(|s| match part {
                    Some(i) => s.parts.get(i).cloned(),
                    None => s.full.clone(),
                }))
            }
            Lookup::Gone => Some(None),
            Lookup::Pending => None,
        })
        .await
        .flatten();
    match found {
        Some(b) => respond(StatusCode::OK, "text/vtt; charset=utf-8", "max-age=60", b),
        None => error(StatusCode::NOT_FOUND),
    }
}

/// The two blocking-reload directives, if present and well formed.
struct Directives {
    msn: Option<u64>,
    part: Option<usize>,
}

fn parse_directives(query: &str) -> Result<Directives, ()> {
    let mut d = Directives { msn: None, part: None };
    for kv in query.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        match k {
            "_HLS_msn" => d.msn = Some(v.parse().map_err(|_| ())?),
            "_HLS_part" => d.part = Some(v.parse().map_err(|_| ())?),
            _ => {}
        }
    }
    if d.part.is_some() && d.msn.is_none() {
        return Err(());
    }
    Ok(d)
}

/// Which media playlist of a stream.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Flavor {
    /// `index.m3u8`: the fMP4 audio/video.
    Media,
    /// `subs.m3u8`: the WebVTT captions, same segments (and, with
    /// `parts`, parts).
    Subtitles { parts: bool },
}

async fn playlist(hls: &Arc<Hls>, name: &str, entry: &Entry, query: &str, token: Option<&str>) -> Response {
    playlist_of(hls, name, entry, query, token, Flavor::Media).await
}

async fn playlist_of(
    hls: &Arc<Hls>,
    name: &str,
    entry: &Entry,
    query: &str,
    token: Option<&str>,
    flavor: Flavor,
) -> Response {
    let Ok(d) = parse_directives(query) else { return error(StatusCode::BAD_REQUEST) };
    if let Some(msn) = d.msn {
        // RFC 8216bis §6.2.5.2: too far past the live edge is a client bug.
        let (open_msn, next_part) = entry.pkg().next_part();
        if msn > open_msn + 2 || (msn == open_msn && d.part.is_some_and(|p| p > next_part + 2)) {
            return error(StatusCode::BAD_REQUEST);
        }
    }
    let wait = entry.block_timeout();
    let body = entry
        .wait(wait, |p| {
            let satisfied = p.ended
                || match (d.msn, d.part) {
                    (None, _) => true,
                    (Some(m), Some(i)) => p.last_part().is_some_and(|last| last >= (m, i)),
                    (Some(m), None) => p.last_complete().is_some_and(|last| last >= m),
                };
            (satisfied && p.ready()).then(|| match flavor {
                Flavor::Media => p.playlist(),
                Flavor::Subtitles { parts } => p.subtitle_playlist(parts),
            })
        })
        .await;
    match body {
        Some(mut b) => {
            // Never on a stream that has ended: its siblings, if any, are the
            // ones still worth switching to. The subtitle rendition has no
            // siblings in its group.
            if !entry.pkg().ended && flavor == Flavor::Media {
                b.push_str(&rendition_reports(hls, name));
            }
            respond(StatusCode::OK, PLAYLIST, "no-cache", with_token(&b, token))
        }
        None => error(StatusCode::SERVICE_UNAVAILABLE),
    }
}

/// `main+480p` belongs to family `main`; `main` belongs to family `main`.
fn family_root(name: &str) -> &str {
    name.split_once('+').map_or(name, |(root, _)| root)
}

/// `#EXT-X-RENDITION-REPORT` for every OTHER live rendition sharing `name`'s
/// family (`<root>` and `<root>+*`), never for `name` itself (Apple -50099).
/// Skips a sibling with no parts yet: there is nothing truthful to report.
fn rendition_reports(hls: &Hls, name: &str) -> String {
    let root = family_root(name);
    let prefix = format!("{root}+");
    let siblings: Vec<(String, Arc<Entry>)> = hls
        .streams()
        .iter()
        .filter(|(n, _)| n.as_str() != name && (n.as_str() == root || n.starts_with(&prefix)))
        .map(|(n, e)| (n.clone(), e.clone()))
        .collect();
    let mut out = String::new();
    for (n, e) in siblings {
        let pkg = e.pkg();
        if pkg.ended {
            continue;
        }
        if let Some((msn, part)) = pkg.last_part() {
            let _ = writeln!(out, "#EXT-X-RENDITION-REPORT:URI=\"../{n}/index.m3u8\",LAST-MSN={msn},LAST-PART={part}");
        }
    }
    out
}

/// `GET /hls/{name}/master.m3u8`. When `name` has no `+`, it is a family
/// root: the master lists it plus every live, ready `{name}+*` rendition,
/// highest `BANDWIDTH` first. When `name` already names one rendition
/// (contains `+`), the master keeps working as a single-variant playlist for
/// just that stream, as it always has.
async fn master(hls: &Arc<Hls>, entry: &Entry, name: &str, requested: &str, token: Option<&str>) -> Response {
    let siblings = || -> Vec<(String, VariantAttrs)> {
        if name.contains('+') {
            return Vec::new();
        }
        let prefix = format!("{name}+");
        let entries: Vec<(String, Arc<Entry>)> =
            hls.streams().iter().filter(|(n, _)| n.starts_with(&prefix)).map(|(n, e)| (n.clone(), e.clone())).collect();
        entries.into_iter().filter_map(|(n, e)| e.pkg().variant_attrs().map(|a| (n, a))).collect()
    };
    // A root whose own media no browser plays (MPEG-2 from a broadcast
    // feed) never becomes a variant; its transcoded `+` renditions are what
    // the master offers then, instead of a 404.
    // Read once, guard dropped: nothing may hold the packager across the wait.
    let (root_now, root_ready) = {
        let p = entry.pkg();
        (p.variant_attrs(), p.ready())
    };
    let mut variants = match root_now {
        Some(a) => vec![(requested.to_owned(), a)],
        None if !root_ready && !siblings().is_empty() => Vec::new(),
        None => {
            let wait = entry.block_timeout();
            match entry.wait(wait, |p| p.variant_attrs()).await {
                Some(a) => vec![(requested.to_owned(), a)],
                None => Vec::new(),
            }
        }
    };
    variants.extend(siblings());
    if variants.is_empty() {
        return error(StatusCode::NOT_FOUND);
    }
    variants.sort_by_key(|(_, a)| std::cmp::Reverse(a.peak));
    let text = hls.text_track(name).map(|t| (family_root(name).to_owned(), t));
    respond(StatusCode::OK, PLAYLIST, "no-cache", with_token(&render_master(&variants, text.as_ref()), token))
}

/// One `#EXT-X-STREAM-INF` + relative `URI` per variant, in the order given
/// (callers sort). The URI is relative to `/hls/{requested-name}/master.m3u8`,
/// so `../{name}/index.m3u8` reaches `/hls/{name}/index.m3u8` for every
/// variant, itself included, keeping tokens and hosts out of it.
///
/// With `text` (the captioned family root and its track), one
/// `#EXT-X-MEDIA:TYPE=SUBTITLES` rendition in group `cc`, which every
/// variant names with `SUBTITLES="cc"`.
fn render_master(variants: &[(String, VariantAttrs)], text: Option<&(String, TextTrack)>) -> String {
    let mut o = String::from("#EXTM3U\n#EXT-X-VERSION:9\n#EXT-X-INDEPENDENT-SEGMENTS\n");
    if let Some((root, t)) = text {
        let _ = write!(o, "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"cc\",NAME=\"{}\"", quoted_safe(&t.name));
        if let Some(lang) = &t.language {
            let _ = write!(o, ",LANGUAGE=\"{}\"", quoted_safe(lang));
        }
        let _ = writeln!(o, ",DEFAULT=NO,AUTOSELECT=YES,FORCED=NO,URI=\"../{root}/subs.m3u8\"");
    }
    for (name, a) in variants {
        let _ = write!(o, "#EXT-X-STREAM-INF:BANDWIDTH={}", a.peak);
        if let Some(avg) = a.average {
            let _ = write!(o, ",AVERAGE-BANDWIDTH={avg}");
        }
        let _ = write!(o, ",CODECS=\"{}\"", a.codecs);
        if let Some((w, h)) = a.resolution {
            let _ = write!(o, ",RESOLUTION={w}x{h}");
        }
        if let Some(fps) = a.frame_rate {
            let _ = write!(o, ",FRAME-RATE={fps:.3}");
        }
        if text.is_some() {
            o.push_str(",SUBTITLES=\"cc\"");
        }
        o.push('\n');
        let _ = writeln!(o, "../{name}/index.m3u8");
    }
    o
}

/// A quoted-string attribute value may not hold `"` or line breaks
/// (RFC 8216 §4.2).
fn quoted_safe(v: &str) -> String {
    v.chars().filter(|c| *c != '"' && *c != '\r' && *c != '\n').collect()
}

/// `?token=` wins over `Authorization: Bearer`, so a player that can only
/// set URLs still works.
fn request_token<'a>(query: &'a str, req: &'a Request) -> Option<&'a str> {
    let from_query = query.split('&').find_map(|kv| kv.strip_prefix("token=")).filter(|t| !t.is_empty());
    from_query.or_else(|| {
        req.headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim)
            .filter(|t| !t.is_empty())
    })
}

/// JWTs are base64url segments joined by dots. Anything else is refused, so
/// a token can never inject text into a playlist.
fn token_is_url_safe(t: &str) -> bool {
    t.len() <= 4096 && t.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Players only put the token on the first playlist request; hls.js does
/// not carry it to the URIs inside. So every URI in a playlist we serve
/// carries it forward: plain URI lines and `URI="..."` attributes.
fn with_token(playlist: &str, token: Option<&str>) -> String {
    let Some(token) = token else { return playlist.to_owned() };
    let add = |uri: &str| format!("{uri}{}token={token}", if uri.contains('?') { '&' } else { '?' });
    let mut out = String::with_capacity(playlist.len() + 64);
    for line in playlist.lines() {
        if line.is_empty() {
        } else if !line.starts_with('#') {
            out.push_str(&add(line));
        } else {
            let mut rest = line;
            while let Some(i) = rest.find("URI=\"") {
                let (head, tail) = rest.split_at(i + 5);
                out.push_str(head);
                let end = tail.find('"').unwrap_or(tail.len());
                out.push_str(&add(&tail[..end]));
                rest = &tail[end..];
            }
            out.push_str(rest);
        }
        out.push('\n');
    }
    out
}

async fn play(State(hls): State<Arc<Hls>>, Path(name): Path<String>) -> Response {
    if !caudal_core::media::valid_stream_name(&name) {
        return error(StatusCode::NOT_FOUND);
    }
    // An unknown stream still gets the page (it retries until the stream
    // appears), but with a 404 status so the answer is honest.
    let known = hls.get(&name).is_some() || hls.registry.get(&name).is_some();
    let status = if known { StatusCode::OK } else { StatusCode::NOT_FOUND };
    respond(status, "text/html; charset=utf-8", "no-cache", Bytes::from_static(PLAY_HTML.as_bytes()))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod ts_tests;

#[cfg(test)]
mod dash_tests;

#[cfg(test)]
mod token_tests {
    use super::{token_is_url_safe, with_token};

    #[test]
    fn every_uri_carries_the_token() {
        let pl = "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXT-X-PART:DURATION=0.2,URI=\"s1.p0.m4s\",INDEPENDENT=YES\n#EXTINF:2.0,\ns1.m4s\n#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"s2.p0.m4s\"\n";
        let out = with_token(pl, Some("a.b.c"));
        assert!(out.contains("URI=\"init.mp4?token=a.b.c\""), "{out}");
        assert!(out.contains("URI=\"s1.p0.m4s?token=a.b.c\",INDEPENDENT=YES"), "{out}");
        assert!(out.contains("\ns1.m4s?token=a.b.c\n"), "{out}");
        assert!(out.contains("URI=\"s2.p0.m4s?token=a.b.c\""), "{out}");
        assert_eq!(with_token(pl, None), pl);
    }

    #[test]
    fn only_jwt_alphabet_tokens() {
        assert!(token_is_url_safe("eyJhbGciOi.J9-_x.sig"));
        assert!(!token_is_url_safe("a\"#EXT-X-ENDLIST"));
        assert!(!token_is_url_safe("a b"));
    }
}
