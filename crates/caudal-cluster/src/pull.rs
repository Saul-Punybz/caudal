//! The edge: pulls a stream from an origin when a viewer first asks for
//! it, and republishes it into the local registry.
//!
//! One task per pulled name. It outlives any one origin: when the MoQ
//! session or broadcast ends it keeps the local [`Publisher`] (so the
//! edge's viewers never restart), tries the other origins starting with
//! the next one, and resumes on the first that has the stream, with the
//! timeline rebased to continue where it stopped.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use bytes::Bytes;
use caudal_core::{BufferConfig, DemandFuture, Frame, PublishError, Publisher, Registry, Stream, TrackKind};
use parking_lot::Mutex;
use tokio::sync::{mpsc, watch};
use url::Url;

use crate::convert::{self, Pulled};
use crate::timing::{Rebase, TrackClock};
use crate::token::Secret;

/// Longest wait for one origin's HTTP answer (locate, fingerprint).
const HTTP_TIMEOUT: Duration = Duration::from_secs(3);
/// Longest wait for the MoQ session, the broadcast and its catalog.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Pause between rounds over the origin list when none had the stream.
const RETRY: Duration = Duration::from_millis(250);
/// No frame from the origin for this long: treat it as gone. A killed
/// origin sends no QUIC close, and the connection's idle timeout (tens of
/// seconds) is far too slow for viewers waiting on the edge.
const STALL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub(crate) struct Settings {
    pub node_id: String,
    pub secret: Secret,
    pub origins: Vec<Url>,
    pub idle_timeout: Duration,
    pub source_timeout: Duration,
    pub buffer: BufferConfig,
}

pub(crate) struct Inner {
    registry: Weak<Registry>,
    cfg: Settings,
    http: reqwest::Client,
    pulls: Mutex<HashMap<String, Arc<Pull>>>,
}

#[derive(Clone)]
enum Ready {
    Pending,
    Up(Arc<Stream>),
    Failed,
}

/// One pulled stream, as `/metrics` sees it.
pub(crate) struct Pull {
    ready: watch::Sender<Ready>,
    origin: Mutex<String>,
    /// Demand (or failover) to first frame, of the latest (re)connection.
    setup: Mutex<Option<Duration>>,
    failovers: AtomicU64,
}

/// What `/metrics` reports for one active pull.
#[derive(Debug, Clone, PartialEq)]
pub struct PullMetric {
    pub stream: String,
    /// The origin currently (or last) pulled from, as configured.
    pub origin: String,
    /// Time from the viewer's demand, or from the last frame the previous
    /// origin sent, to the first frame republished locally from the current
    /// one. `None` until then.
    pub setup: Option<Duration>,
    pub failovers: u64,
}

impl Inner {
    pub(crate) fn new(registry: &Arc<Registry>, cfg: Settings) -> Arc<Self> {
        let http = reqwest::Client::builder().timeout(HTTP_TIMEOUT).build().expect("reqwest client");
        Arc::new(Self { registry: Arc::downgrade(registry), cfg, http, pulls: Mutex::default() })
    }

    pub(crate) fn metrics(&self) -> Vec<PullMetric> {
        let mut v: Vec<PullMetric> = self
            .pulls
            .lock()
            .iter()
            .map(|(name, p)| PullMetric {
                stream: name.clone(),
                origin: p.origin.lock().clone(),
                setup: *p.setup.lock(),
                failovers: p.failovers.load(Ordering::Relaxed),
            })
            .collect();
        v.sort_by(|a, b| a.stream.cmp(&b.stream));
        v
    }

    pub(crate) fn demand<'a>(self: &'a Arc<Self>, name: &'a str) -> DemandFuture<'a> {
        Box::pin(async move {
            let pull = {
                let mut map = self.pulls.lock();
                match map.get(name) {
                    Some(p) => p.clone(),
                    None => {
                        let p = Arc::new(Pull {
                            ready: watch::channel(Ready::Pending).0,
                            origin: Mutex::new(String::new()),
                            setup: Mutex::new(None),
                            failovers: AtomicU64::new(0),
                        });
                        map.insert(name.to_owned(), p.clone());
                        tokio::spawn(run(self.clone(), name.to_owned(), p.clone()));
                        p
                    }
                }
            };
            let mut rx = pull.ready.subscribe();
            let ready = rx.wait_for(|r| !matches!(r, Ready::Pending)).await.ok()?.clone();
            match ready {
                Ready::Up(s) => Some(s),
                _ => None,
            }
        })
    }

    fn token(&self) -> String {
        self.cfg.secret.mint(&self.cfg.node_id)
    }
}

/// Where to subscribe on one origin.
struct Target {
    index: usize,
    moq: Url,
    fingerprint: Option<String>,
}

#[derive(serde::Deserialize)]
struct Fingerprint {
    url: String,
    fingerprint: Option<String>,
}

/// Asks origin `index` whether it has `name` live and where its MoQ
/// endpoint is.
async fn locate(inner: &Inner, index: usize, name: &str) -> Result<Target, String> {
    let base = inner.cfg.origins[index].as_str().trim_end_matches('/');
    let res = inner
        .http
        .get(format!("{base}/api/v1/cluster/locate/{name}"))
        .bearer_auth(inner.token())
        .send()
        .await
        .map_err(|e| format!("locate: {e}"))?;
    match res.status().as_u16() {
        200 => {}
        404 => return Err("not live there".into()),
        401 | 403 => return Err(format!("locate refused ({}): check [cluster] secret", res.status())),
        s => return Err(format!("locate: HTTP {s}")),
    }
    let fp: Fingerprint = inner
        .http
        .get(format!("{base}/moq/fingerprint"))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("moq endpoint: {e}"))?
        .json()
        .await
        .map_err(|e| format!("moq endpoint: {e}"))?;
    let moq = Url::parse(&fp.url).map_err(|e| format!("moq url {}: {e}", fp.url))?;
    Ok(Target { index, moq, fingerprint: fp.fingerprint })
}

/// The first origin, starting at `start` and wrapping around, that has
/// `name` live.
async fn locate_any(inner: &Inner, name: &str, start: usize) -> Option<Target> {
    let n = inner.cfg.origins.len();
    for k in 0..n {
        let i = (start + k) % n;
        match locate(inner, i, name).await {
            Ok(t) => return Some(t),
            Err(e) => {
                tracing::debug!(stream = name, origin = %inner.cfg.origins[i], error = %e, "cluster: origin skipped")
            }
        }
    }
    None
}

/// Why a session with one origin stopped.
enum Outcome {
    /// Nobody watched for `idle_timeout`: stop pulling.
    Idle,
    /// The origin went away or stopped sending: try the others.
    Lost(String),
    /// The origin's track list changed: subscribe again, same origin.
    Reload,
    /// Someone published `name` on this node meanwhile.
    Busy,
}

/// Everything that outlives one origin session.
struct Local {
    publisher: Option<Publisher>,
    tracks: Vec<Pulled>,
    clocks: Vec<TrackClock>,
    rebase: Rebase,
    last_viewer: Instant,
    /// Since when no origin has been sending (cleared by the first frame of
    /// a new session).
    lost_at: Option<Instant>,
    /// When the last frame from any origin was republished.
    last_frame: Instant,
}

async fn run(inner: Arc<Inner>, name: String, pull: Arc<Pull>) {
    let t0 = Instant::now();
    let mut local = Local {
        publisher: None,
        tracks: Vec::new(),
        clocks: Vec::new(),
        rebase: Rebase::default(),
        last_viewer: Instant::now(),
        lost_at: None,
        last_frame: Instant::now(),
    };
    let mut next = 0;
    // When the current attempt started: the demand, or losing the source.
    let mut attempt = t0;
    loop {
        if inner.registry.strong_count() == 0 {
            break;
        }
        if local.publisher.as_ref().is_some_and(|p| p.stream().stats().viewers > 0) {
            local.last_viewer = Instant::now();
        }
        if local.publisher.is_some() && local.last_viewer.elapsed() >= inner.cfg.idle_timeout {
            tracing::info!(stream = %name, "cluster: no viewers; pull stopped");
            break;
        }
        let Some(target) = locate_any(&inner, &name, next).await else {
            // A first viewer of a name no origin has gets a quick 404; a
            // stream already serving viewers waits for its source a while.
            if local.publisher.is_none() || local.lost_at.unwrap_or(t0).elapsed() >= inner.cfg.source_timeout {
                tracing::info!(stream = %name, "cluster: no origin has the stream");
                break;
            }
            tokio::time::sleep(RETRY).await;
            continue;
        };
        let origin = inner.cfg.origins[target.index].to_string();
        *pull.origin.lock() = origin.clone();
        match session(&inner, &name, &target, &pull, &mut local, attempt).await {
            Outcome::Idle => {
                tracing::info!(stream = %name, "cluster: no viewers; pull stopped");
                break;
            }
            Outcome::Busy => break,
            Outcome::Reload => {
                next = target.index;
                attempt = Instant::now();
            }
            Outcome::Lost(why) => {
                tracing::warn!(stream = %name, %origin, reason = %why, "cluster: lost the origin; failing over");
                next = target.index + 1;
                if local.publisher.is_some() {
                    if local.lost_at.is_none() {
                        local.lost_at = Some(local.last_frame);
                        pull.failovers.fetch_add(1, Ordering::Relaxed);
                        local.rebase.new_source();
                    }
                    attempt = local.lost_at.unwrap_or(t0);
                } else if t0.elapsed() >= caudal_core::DEMAND_TIMEOUT {
                    break;
                }
                tokio::time::sleep(RETRY / 5).await;
            }
        }
        if local.publisher.is_some() && local.lost_at.is_some_and(|l| l.elapsed() >= inner.cfg.source_timeout) {
            tracing::info!(stream = %name, "cluster: source lost for too long; stream ended");
            break;
        }
    }
    // Leave the map before announcing the end, so a new demand starts a
    // new pull instead of joining this finished one.
    {
        let mut map = inner.pulls.lock();
        if map.get(&name).is_some_and(|p| Arc::ptr_eq(p, &pull)) {
            map.remove(&name);
        }
    }
    let busy = inner.registry.upgrade().and_then(|r| r.get(&name)).filter(|_| local.publisher.is_none());
    pull.ready.send_modify(|r| {
        if matches!(r, Ready::Pending) {
            *r = busy.map_or(Ready::Failed, Ready::Up);
        }
    });
    drop(local.publisher);
}

/// A frame read from one MoQ track: track index, PTS in µs, first frame
/// of its group, payload.
type Raw = (usize, i64, bool, Bytes);

async fn session(
    inner: &Arc<Inner>,
    name: &str,
    target: &Target,
    pull: &Pull,
    local: &mut Local,
    attempt: Instant,
) -> Outcome {
    let Some(registry) = inner.registry.upgrade() else { return Outcome::Idle };

    let mut client_cfg = moq_native::ClientConfig::default();
    if let Some(fp) = &target.fingerprint {
        client_cfg.tls.fingerprint = vec![fp.clone()];
    }
    let client = match client_cfg.init() {
        Ok(c) => c,
        Err(e) => return Outcome::Lost(format!("moq client: {e}")),
    };
    let mut url = target.moq.clone();
    url.set_path(&format!("/{name}"));
    url.set_query(Some(&format!("jwt={}", inner.token())));

    let sub_origin = moq_net::Origin::random().produce();
    let moq_session =
        match tokio::time::timeout(CONNECT_TIMEOUT, client.with_subscriber(sub_origin.clone()).connect(url)).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => return Outcome::Lost(format!("moq connect: {e}")),
            Err(_) => return Outcome::Lost("moq connect timed out".into()),
        };
    let bc = match tokio::time::timeout(CONNECT_TIMEOUT, sub_origin.consume().announced_broadcast(name)).await {
        Ok(Some(bc)) => bc,
        _ => return Outcome::Lost("broadcast not announced".into()),
    };
    let mut catalog_track = match bc.track(hang::Catalog::DEFAULT_NAME) {
        Ok(t) => match tokio::time::timeout(CONNECT_TIMEOUT, t.subscribe(None)).await {
            Ok(Ok(s)) => s,
            _ => return Outcome::Lost("catalog subscribe failed".into()),
        },
        Err(e) => return Outcome::Lost(format!("catalog: {e}")),
    };
    let tracks = match tokio::time::timeout(CONNECT_TIMEOUT, read_catalog(&mut catalog_track)).await {
        Ok(Some(t)) if !t.is_empty() => t,
        Ok(Some(_)) => return Outcome::Lost("no track the edge can carry".into()),
        _ => return Outcome::Lost("catalog not received".into()),
    };

    // The local stream: created on the first session, kept across the
    // following ones.
    if local.publisher.is_none() {
        match registry.publish(name, inner.cfg.buffer) {
            Ok(p) => local.publisher = Some(p),
            Err(PublishError::Busy(_)) => return Outcome::Busy,
            Err(e) => return Outcome::Lost(e.to_string()),
        }
    }
    let publisher = local.publisher.as_ref().expect("just set");
    if local.tracks != tracks {
        let infos = tracks.iter().map(|t| t.info.clone()).collect();
        if let Err(e) = publisher.set_tracks(infos) {
            return Outcome::Lost(e.to_string());
        }
        local.clocks =
            tracks.iter().map(|t| TrackClock::new(t.info.timescale, t.info.kind() == TrackKind::Video)).collect();
        local.tracks = tracks.clone();
    }
    let stream = publisher.stream().clone();
    pull.ready.send_modify(|r| *r = Ready::Up(stream.clone()));
    tracing::info!(stream = name, origin = %inner.cfg.origins[target.index], tracks = tracks.len(), "cluster: pulling");

    // One reader per track, all feeding this loop; they stop when the set
    // is dropped.
    let (tx, mut rx) = mpsc::channel::<Raw>(512);
    let mut readers = tokio::task::JoinSet::new();
    for (i, t) in tracks.iter().enumerate() {
        let track = match bc.track(&t.moq_name) {
            Ok(t) => t,
            Err(e) => return Outcome::Lost(format!("track {}: {e}", t.moq_name)),
        };
        let tx = tx.clone();
        readers.spawn(async move {
            let Ok(sub) = track.subscribe(None).await else { return };
            read_track(sub, i, tx).await;
        });
    }
    drop(tx);
    let (catalog_tx, mut catalog_rx) = tokio::sync::oneshot::channel();
    {
        let current = tracks.clone();
        readers.spawn(async move {
            let _ = catalog_tx.send(catalog_changed(&mut catalog_track, &current).await);
        });
    }

    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut first = true;
    local.last_frame = Instant::now();
    loop {
        tokio::select! {
            raw = rx.recv() => {
                let Some((i, pts_us, group_start, data)) = raw else {
                    return Outcome::Lost("every track ended".into());
                };
                let info = &local.tracks[i].info;
                let video = info.kind() == TrackKind::Video;
                let (dts, pts) = local.clocks[i].stamp(local.rebase.map(pts_us));
                let frame = Frame { track: info.id, dts, pts, keyframe: !video || group_start, data };
                if let Err(e) = publisher.push(frame) {
                    return Outcome::Lost(e.to_string());
                }
                local.last_frame = Instant::now();
                if first {
                    first = false;
                    local.lost_at = None;
                    *pull.setup.lock() = Some(attempt.elapsed());
                }
            }
            _ = bc.closed() => return Outcome::Lost("broadcast ended".into()),
            e = moq_session.closed() => return Outcome::Lost(format!("session closed: {e}")),
            changed = &mut catalog_rx => {
                if changed.unwrap_or(false) {
                    tracing::info!(stream = name, "cluster: origin's tracks changed; resubscribing");
                    return Outcome::Reload;
                }
                return Outcome::Lost("catalog ended".into());
            }
            _ = tick.tick() => {
                if local.last_frame.elapsed() >= STALL {
                    return Outcome::Lost(format!("no frame for {STALL:?}"));
                }
                if stream.stats().viewers > 0 {
                    local.last_viewer = Instant::now();
                } else if local.last_viewer.elapsed() >= inner.cfg.idle_timeout {
                    return Outcome::Idle;
                }
            }
        }
    }
}

/// The next catalog on the catalog track, as pulled tracks.
async fn read_catalog(track: &mut moq_net::track::Subscriber) -> Option<Vec<Pulled>> {
    let mut group = track.next_group().await.ok()??;
    let frame = group.read_frame().await.ok()??;
    let catalog = hang::Catalog::from_slice(&frame.payload).ok()?;
    Some(convert::tracks(&catalog))
}

/// Waits for a catalog that differs from `current`: `true` then, `false`
/// when the catalog track ends.
async fn catalog_changed(track: &mut moq_net::track::Subscriber, current: &[Pulled]) -> bool {
    loop {
        match read_catalog(track).await {
            Some(t) if t != current => return true,
            Some(_) => {}
            None => return false,
        }
    }
}

async fn read_track(mut sub: moq_net::track::Subscriber, index: usize, tx: mpsc::Sender<Raw>) {
    while let Ok(Some(mut group)) = sub.next_group().await {
        let mut first = true;
        while let Ok(Some(f)) = group.read_frame().await {
            let Ok(frame) = hang::container::Frame::decode(f.payload) else { continue };
            let pts = i64::try_from(frame.timestamp.as_micros()).unwrap_or(i64::MAX);
            if tx.send((index, pts, first, frame.payload)).await.is_err() {
                return;
            }
            first = false;
        }
    }
}
