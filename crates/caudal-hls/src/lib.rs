//! LL-HLS / CMAF output and the `/play/{name}` page. Entry point fixed by
//! the orchestrator.
//!
//! One packager task per published stream turns frames into CMAF parts as
//! they arrive, so parts exist before the first viewer asks. HTTP handlers
//! only read the packager state; blocking playlist reloads and requests for
//! the preload-hinted part wait on a `watch` channel bumped by the packager.

mod fmp4;
mod packager;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bytes::Bytes;
use caudal_core::{Event, Registry, StartAt, Stream, Subscriber};
use tokio::runtime::Handle;
use tokio::sync::{broadcast, watch};

use packager::{Lookup, Packager};

#[derive(Debug, Clone, Copy)]
pub struct HlsConfig {
    /// Target partial-segment duration.
    pub part_ms: u32,
    /// Target full-segment duration.
    pub segment_ms: u32,
}

/// How long an ended stream keeps answering (with `#EXT-X-ENDLIST`).
const LINGER: Duration = Duration::from_secs(30);

const PLAY_HTML: &str = include_str!("../static/play.html");

/// Serves `/hls/{name}/index.m3u8`, `/hls/{name}/init.mp4`,
/// `/hls/{name}/{segment}.m4s` and `/play/{name}`. Mounted at the root by
/// the server; paths are absolute.
///
/// Must be called inside a tokio runtime: it spawns the task that starts a
/// packager for every stream that gets published.
pub fn router(registry: Arc<Registry>, cfg: HlsConfig) -> axum::Router {
    let cfg = HlsConfig { part_ms: cfg.part_ms.max(10), segment_ms: cfg.segment_ms.max(cfg.part_ms.max(10)) };
    let hls = Arc::new(Hls { registry: registry.clone(), cfg, streams: Mutex::default() });
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
    Router::new().route("/hls/{name}/{file}", get(hls_file)).route("/play/{name}", get(play)).with_state(hls)
}

struct Hls {
    registry: Arc<Registry>,
    cfg: HlsConfig,
    streams: Mutex<HashMap<String, Arc<Entry>>>,
}

/// One stream's packager and the channel its waiters sleep on.
struct Entry {
    stream: Arc<Stream>,
    pkg: Mutex<Packager>,
    tick: watch::Sender<u64>,
}

impl Entry {
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
            if map.get(stream.name()).is_some_and(|e| Arc::ptr_eq(&e.stream, &stream)) {
                return;
            }
            let entry = Arc::new(Entry {
                stream: stream.clone(),
                pkg: Mutex::new(Packager::new(self.cfg)),
                tick: watch::channel(0).0,
            });
            map.insert(stream.name().to_owned(), entry.clone());
            entry
        };
        // Subscribe now, not when the task first runs, so no frame pushed
        // in between is missed.
        let sub = stream.subscribe(StartAt::LiveEdge);
        tracing::debug!(stream = %stream.name(), "ll-hls packager started");
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

/// The packager task for one stream.
async fn run(hls: Arc<Hls>, entry: Arc<Entry>, mut sub: Subscriber) {
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
                    tracing::warn!(stream = %entry.stream.name(), skipped, "ll-hls packager lagged");
                    pkg.lagged();
                }
                Event::End => pkg.end(),
            }
            pkg.ended || before != (pkg.last_part(), pkg.segments.len(), pkg.init.is_some())
        };
        if changed {
            entry.tick.send_modify(|v| *v = v.wrapping_add(1));
        }
        if ev == Event::End {
            break;
        }
    }
    drop(sub);
    tokio::time::sleep(LINGER).await;
    let mut map = hls.streams();
    if map.get(entry.stream.name()).is_some_and(|e| Arc::ptr_eq(e, &entry)) {
        map.remove(entry.stream.name());
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
) -> Response {
    let Some(entry) = hls.get(&name) else { return error(StatusCode::NOT_FOUND) };
    match file.as_str() {
        "index.m3u8" => playlist(&entry, query.as_deref().unwrap_or("")).await,
        "init.mp4" => {
            let wait = entry.block_timeout();
            match entry.wait(wait, |p| p.init.clone()).await {
                Some(init) => respond(StatusCode::OK, "video/mp4", "no-cache", init),
                None => error(StatusCode::NOT_FOUND),
            }
        }
        other => match parse_media_name(other) {
            Some((msn, part)) => media(&entry, msn, part).await,
            None => error(StatusCode::NOT_FOUND),
        },
    }
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

async fn playlist(entry: &Entry, query: &str) -> Response {
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
            (satisfied && p.ready()).then(|| p.playlist())
        })
        .await;
    match body {
        Some(b) => respond(StatusCode::OK, PLAYLIST, "no-cache", b),
        None => error(StatusCode::SERVICE_UNAVAILABLE),
    }
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
