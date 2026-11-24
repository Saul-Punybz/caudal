//! Stream names to live streams, with one publisher per name.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::{Mutex, RwLock};
use tokio::sync::broadcast;

use crate::gate::{Access, Denied, Gate, Rewrite, RewriteRequest, StreamKeys};
use crate::media::{Cue, Frame, TrackInfo, valid_stream_name};
use crate::session::{Session, Sessions};
use crate::stream::{BufferConfig, PushError, StartAt, Stream, Subscriber};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PublishError {
    #[error("invalid stream name")]
    InvalidName,
    #[error("stream {0} already has a publisher")]
    Busy(String),
}

pub struct Registry {
    streams: Mutex<HashMap<Arc<str>, Arc<Stream>>>,
    published: broadcast::Sender<Arc<Stream>>,
    ended: broadcast::Sender<Arc<str>>,
    // `dyn Gate` can't live behind `arc-swap` (it needs `T: Sized`), so a
    // short-held read/write lock stands in; `authorize` is called per
    // publish/play request, never per frame, so this is not the hot path
    // rule 3 in the reload brief means to keep lock-free.
    gate: RwLock<Option<Arc<dyn Gate>>>,
    /// Name routing asked before the gate (see [`Registry::resolve`]).
    /// Same locking rationale as `gate`.
    rewrite: RwLock<Option<Arc<dyn Rewrite>>>,
    /// Stream keys asked before the rewrite (see [`Registry::resolve`]).
    /// Same locking rationale as `gate`.
    keys: RwLock<Option<Arc<dyn StreamKeys>>>,
    /// Where a viewer's request for an unknown name goes (a cluster edge
    /// pulling from its origin). Same locking rationale as `gate`.
    demand: RwLock<Option<Arc<dyn Demand>>>,
    /// Open viewer sessions and kick blocks.
    sessions: Arc<Sessions>,
}

/// Longest [`Registry::get_or_demand`] waits for a [`Demand`] to bring a
/// stream up, whatever the demand source does.
pub const DEMAND_TIMEOUT: Duration = Duration::from_secs(15);

pub type DemandFuture<'a> = Pin<Box<dyn Future<Output = Option<Arc<Stream>>> + Send + 'a>>;

/// A source of streams on demand: asked when a viewer wants a name that
/// no publisher is feeding. A cluster edge implements it by pulling the
/// stream from an origin and publishing it into this registry as a normal
/// stream, so every output serves it unchanged.
pub trait Demand: Send + Sync + 'static {
    /// Brings `name` up if it can and resolves to the published stream, or
    /// `None` when no source has it. Called concurrently for the same name
    /// by many viewers; implementations start one source per name.
    fn demand<'a>(&'a self, name: &'a str) -> DemandFuture<'a>;
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            streams: Mutex::default(),
            published: broadcast::channel(64).0,
            ended: broadcast::channel(64).0,
            gate: RwLock::new(None),
            rewrite: RwLock::new(None),
            keys: RwLock::new(None),
            demand: RwLock::new(None),
            sessions: Arc::default(),
        }
    }
}

impl Registry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Every stream that starts publishing from now on. Outputs that must
    /// run ahead of their first viewer (LL-HLS) start their packager here.
    pub fn subscribe_publishes(&self) -> broadcast::Receiver<Arc<Stream>> {
        self.published.subscribe()
    }

    /// Names of streams whose publisher went away, from now on.
    pub fn subscribe_ends(&self) -> broadcast::Receiver<Arc<str>> {
        self.ended.subscribe()
    }

    /// Installs (or replaces, e.g. on a config reload) the access policy.
    /// Without one, every publish and play is allowed. A live snapshot
    /// swap: in-flight `authorize` calls finish against whichever gate they
    /// loaded, never torn mid-check.
    pub fn set_gate(&self, gate: Arc<dyn Gate>) {
        *self.gate.write() = Some(gate);
    }

    /// Removes the access policy: every publish and play becomes allowed
    /// again (used when a reload drops `[auth]`'s keys).
    pub fn clear_gate(&self) {
        *self.gate.write() = None;
    }

    /// Installs (or replaces, on a reload) the name routing every
    /// [`Registry::resolve`] asks first.
    pub fn set_rewrite(&self, rewrite: Arc<dyn Rewrite>) {
        *self.rewrite.write() = Some(rewrite);
    }

    /// Removes the name routing: every name resolves to itself again.
    pub fn clear_rewrite(&self) {
        *self.rewrite.write() = None;
    }

    /// Installs (or replaces, on a reload) the stream key table every
    /// [`Registry::resolve`] asks first.
    pub fn set_stream_keys(&self, keys: Arc<dyn StreamKeys>) {
        *self.keys.write() = Some(keys);
    }

    /// Removes the stream key table.
    pub fn clear_stream_keys(&self) {
        *self.keys.write() = None;
    }

    /// `requested` as it may be logged: `"****"` when it is a stream key,
    /// unchanged otherwise. Protocols log the name a client sent through
    /// this when they refuse a request.
    pub fn loggable<'a>(&self, requested: &'a str) -> std::borrow::Cow<'a, str> {
        match self.keys.read().as_ref() {
            Some(k) if k.is_key(requested) => std::borrow::Cow::Borrowed("****"),
            _ => std::borrow::Cow::Borrowed(requested),
        }
    }

    /// The name a publish or play request really goes to, after the
    /// stream keys and the installed [`Rewrite`] (if any) and the gate.
    /// Protocols call this with the name the client sent and use the
    /// answer for `publish`, `get_or_demand` and `open_session`.
    ///
    /// Order: a stream key (sent as the name, or as the token for its own
    /// stream) resolves straight to its stream, skips the rewrite and
    /// stands in for the token; otherwise the rewrite routes the name.
    /// The gate judges the *final* name either way: neither a key nor a
    /// rewrite can route a client around `[[access.rules]]` or
    /// `[auth.hook]`, and a rewrite cannot get around tokens. A rewritten
    /// name must be a valid stream name or the request is refused.
    pub async fn resolve(
        &self,
        access: Access,
        protocol: &'static str,
        stream: &str,
        token: Option<&str>,
        ip: Option<std::net::IpAddr>,
    ) -> Result<String, Denied> {
        let keys = self.keys.read().clone();
        if let Some(keys) = &keys {
            // Never log `stream` here: it may be the key.
            if let Some(name) = keys.by_name(access, stream) {
                self.authorize_inner(access, &name, None, ip, true).await?;
                tracing::debug!(stream = %name, protocol, action = ?access, "stream key accepted");
                return Ok(name);
            }
            if let Some(t) = token
                && keys.by_token(access, t, stream)
            {
                self.authorize_inner(access, stream, None, ip, true).await?;
                tracing::debug!(stream, protocol, action = ?access, "stream key accepted");
                return Ok(stream.to_owned());
            }
        }
        let rewrite = self.rewrite.read().clone();
        let name = match rewrite {
            Some(r) => {
                let name = r.rewrite(RewriteRequest { access, protocol, stream, token, ip }).await?;
                if name != stream && !valid_stream_name(&name) {
                    tracing::warn!(requested = stream, rewritten = %name, "rewrite returned an invalid stream name");
                    return Err(Denied::Refused("rewritten to an invalid stream name".into()));
                }
                if name != stream {
                    tracing::debug!(requested = stream, stream = %name, protocol, action = ?access, "stream name rewritten");
                }
                name
            }
            None => stream.to_owned(),
        };
        if let Some(keys) = &keys
            && keys.requires_key(access, &name)
        {
            return Err(Denied::Refused("this stream only takes its stream key".into()));
        }
        self.authorize(access, &name, token, ip).await?;
        Ok(name)
    }

    /// Asks the installed gate, without the rewrite: protocols call
    /// [`Registry::resolve`] for a new publish or play, and this to
    /// re-check a name they already resolved (RTSP `PLAY`) or for requests
    /// that are not routed (control endpoints, recordings). `ip` is the caller's resolved address, when
    /// the protocol has one to give (see [`Gate::check`]).
    pub async fn authorize(
        &self,
        access: Access,
        stream: &str,
        token: Option<&str>,
        ip: Option<std::net::IpAddr>,
    ) -> Result<(), Denied> {
        self.authorize_inner(access, stream, token, ip, false).await
    }

    /// [`Registry::authorize`]; `keyed` when a stream key stood in for
    /// the token (see [`Gate::check_keyed`]).
    async fn authorize_inner(
        &self,
        access: Access,
        stream: &str,
        token: Option<&str>,
        ip: Option<std::net::IpAddr>,
        keyed: bool,
    ) -> Result<(), Denied> {
        if access == Access::Play
            && let Some(ip) = ip
            && self.sessions.is_blocked(stream, ip)
        {
            return Err(Denied::Refused("kicked from this stream; try again later".into()));
        }
        let gate = self.gate.read().clone();
        match gate {
            Some(g) if keyed => g.check_keyed(access, stream, ip).await,
            Some(g) => g.check(access, stream, token, ip).await,
            None => Ok(()),
        }
    }

    /// Re-checks a request [`Registry::resolve`] already turned from
    /// `requested` into `resolved` (RTSP `PLAY` after `DESCRIBE`): the gate
    /// again, the rewrite not, and a stream key still standing in for the
    /// token when `requested` was one.
    pub async fn recheck(
        &self,
        access: Access,
        requested: &str,
        resolved: &str,
        token: Option<&str>,
        ip: Option<std::net::IpAddr>,
    ) -> Result<(), Denied> {
        let keys = self.keys.read().clone();
        if let Some(keys) = &keys {
            let by_name = keys.by_name(access, requested).is_some_and(|s| s == resolved);
            let by_token = requested == resolved && token.is_some_and(|t| keys.by_token(access, t, resolved));
            if by_name || by_token {
                return self.authorize_inner(access, resolved, None, ip, true).await;
            }
            if keys.requires_key(access, resolved) {
                return Err(Denied::Refused("this stream only takes its stream key".into()));
            }
        }
        self.authorize(access, resolved, token, ip).await
    }

    /// Opens a viewer session for `stream` over `protocol`; it lasts until
    /// the returned [`Session`] is dropped. See [`crate::session`].
    pub fn open_session(&self, stream: &str, protocol: &'static str, ip: Option<std::net::IpAddr>) -> Session {
        self.sessions.open(stream.into(), protocol, ip)
    }

    /// Open sessions and kicks, for the API.
    pub fn sessions(&self) -> &Sessions {
        &self.sessions
    }

    /// Claims `name` for a new publisher. The claim lasts until the returned
    /// [`Publisher`] is dropped.
    pub fn publish(self: &Arc<Self>, name: &str, cfg: BufferConfig) -> Result<Publisher, PublishError> {
        if !valid_stream_name(name) {
            return Err(PublishError::InvalidName);
        }
        let mut map = self.streams.lock();
        if map.contains_key(name) {
            return Err(PublishError::Busy(name.to_owned()));
        }
        let name: Arc<str> = name.into();
        let stream = Stream::new(name.clone(), cfg);
        map.insert(name, stream.clone());
        drop(map);
        let _ = self.published.send(stream.clone());
        tracing::info!(stream = %stream.name(), "publish started");
        Ok(Publisher { registry: self.clone(), stream })
    }

    pub fn get(&self, name: &str) -> Option<Arc<Stream>> {
        self.streams.lock().get(name).cloned()
    }

    /// Installs the source asked for streams nobody publishes here (see
    /// [`Demand`]). Without one, [`Registry::get_or_demand`] is `get`.
    pub fn set_demand(&self, demand: Arc<dyn Demand>) {
        *self.demand.write() = Some(demand);
    }

    /// The live stream `name`; if there is none, asks the installed
    /// [`Demand`] source and waits (at most [`DEMAND_TIMEOUT`]) for it to
    /// publish. Outputs call this for a viewer who already passed
    /// `authorize`, so an unauthorized request never starts a pull.
    pub async fn get_or_demand(&self, name: &str) -> Option<Arc<Stream>> {
        if let Some(s) = self.get(name) {
            return Some(s);
        }
        if !valid_stream_name(name) {
            return None;
        }
        let demand = self.demand.read().clone()?;
        match tokio::time::timeout(DEMAND_TIMEOUT, demand.demand(name)).await {
            Ok(Some(s)) if !s.is_ended() => Some(s),
            _ => self.get(name),
        }
    }

    pub fn subscribe(&self, name: &str, start: StartAt) -> Option<Subscriber> {
        self.get(name).map(|s| s.subscribe(start))
    }

    /// Every live stream, sorted by name.
    pub fn list(&self) -> Vec<Arc<Stream>> {
        let mut v: Vec<_> = self.streams.lock().values().cloned().collect();
        v.sort_by(|a, b| a.name().cmp(b.name()));
        v
    }
}

/// The write side of a stream. Dropping it ends the stream: viewers drain
/// what is buffered and then receive `Event::End`.
pub struct Publisher {
    registry: Arc<Registry>,
    stream: Arc<Stream>,
}

impl Publisher {
    pub fn stream(&self) -> &Arc<Stream> {
        &self.stream
    }

    /// Announces (or replaces) the track list. Frames may only reference
    /// announced tracks.
    pub fn set_tracks(&self, tracks: Vec<TrackInfo>) -> Result<(), PushError> {
        self.stream.set_tracks(tracks)
    }

    pub fn push(&self, frame: Frame) -> Result<(), PushError> {
        self.stream.push(frame)
    }

    /// Pushes an SCTE-35 cue that arrived with the media (TS PID 0x86,
    /// RTMP `onCuePoint`). Same path as [`Stream::inject_cue`].
    pub fn push_cue(&self, cue: Cue) -> Result<(), PushError> {
        self.stream.inject_cue(cue)
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        self.stream.end();
        let mut map = self.registry.streams.lock();
        if map.get(self.stream.name()).is_some_and(|s| Arc::ptr_eq(s, &self.stream)) {
            map.remove(self.stream.name());
        }
        drop(map);
        let _ = self.registry.ended.send(self.stream.name().into());
        tracing::info!(stream = %self.stream.name(), "publish ended");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Weak;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Publishes any name it is asked for, and keeps the publisher.
    struct Source {
        registry: Weak<Registry>,
        asked: AtomicUsize,
        held: Mutex<Vec<Publisher>>,
    }

    impl Demand for Source {
        fn demand<'a>(&'a self, name: &'a str) -> DemandFuture<'a> {
            Box::pin(async move {
                self.asked.fetch_add(1, Ordering::Relaxed);
                let p = self.registry.upgrade()?.publish(name, BufferConfig::default()).ok()?;
                let s = p.stream().clone();
                self.held.lock().push(p);
                Some(s)
            })
        }
    }

    #[tokio::test]
    async fn get_or_demand() {
        let reg = Registry::new();
        assert!(reg.get_or_demand("cam").await.is_none(), "no demand source: plain get");

        let src =
            Arc::new(Source { registry: Arc::downgrade(&reg), asked: AtomicUsize::new(0), held: Mutex::default() });
        reg.set_demand(src.clone());
        let s = reg.get_or_demand("cam").await.expect("brought up on demand");
        assert!(Arc::ptr_eq(&s, &reg.get("cam").unwrap()));
        // Live now: no second demand.
        assert!(Arc::ptr_eq(&s, &reg.get_or_demand("cam").await.unwrap()));
        assert_eq!(src.asked.load(Ordering::Relaxed), 1);
        // Names that can never exist are not asked for.
        assert!(reg.get_or_demand("../x").await.is_none());
        assert_eq!(src.asked.load(Ordering::Relaxed), 1);
    }

    /// `alias` -> `real`, `gone` -> refused, anything else kept.
    struct Router;

    impl Rewrite for Router {
        fn rewrite<'a>(&'a self, req: RewriteRequest<'a>) -> crate::gate::RewriteFuture<'a> {
            Box::pin(async move {
                match req.stream {
                    "alias" => Ok("real".to_owned()),
                    "bad" => Ok("../etc".to_owned()),
                    "gone" => Err(Denied::Refused("no route".into())),
                    other => Ok(other.to_owned()),
                }
            })
        }
    }

    /// Refuses every play of `real`.
    struct NoReal;

    impl Gate for NoReal {
        fn check<'a>(
            &'a self,
            access: Access,
            stream: &'a str,
            _token: Option<&'a str>,
            _ip: Option<std::net::IpAddr>,
        ) -> crate::gate::GateFuture<'a> {
            Box::pin(async move {
                if access == Access::Play && stream == "real" { Err(Denied::Refused("no".into())) } else { Ok(()) }
            })
        }
    }

    #[tokio::test]
    async fn resolve_rewrites_then_gates_the_final_name() {
        let reg = Registry::new();
        assert_eq!(reg.resolve(Access::Play, "hls", "alias", None, None).await, Ok("alias".into()), "no rewrite");
        reg.set_rewrite(Arc::new(Router));
        assert_eq!(reg.resolve(Access::Publish, "rtmp", "alias", None, None).await, Ok("real".into()));
        assert_eq!(reg.resolve(Access::Play, "hls", "cam", None, None).await, Ok("cam".into()));
        assert!(reg.resolve(Access::Play, "hls", "gone", None, None).await.is_err());
        assert!(reg.resolve(Access::Play, "hls", "bad", None, None).await.is_err(), "invalid rewritten name");
        reg.set_gate(Arc::new(NoReal));
        assert!(reg.resolve(Access::Play, "hls", "alias", None, None).await.is_err(), "the gate sees `real`");
        assert_eq!(reg.resolve(Access::Publish, "rtmp", "alias", None, None).await, Ok("real".into()));
        reg.clear_rewrite();
        assert_eq!(reg.resolve(Access::Play, "hls", "alias", None, None).await, Ok("alias".into()));
    }

    /// `k3y` publishes to `real`; `real` takes its key only.
    struct Keys;

    impl StreamKeys for Keys {
        fn by_name(&self, access: Access, presented: &str) -> Option<String> {
            (access == Access::Publish && presented == "k3y").then(|| "real".to_owned())
        }
        fn by_token(&self, access: Access, token: &str, stream: &str) -> bool {
            access == Access::Publish && token == "k3y" && stream == "real"
        }
        fn requires_key(&self, access: Access, stream: &str) -> bool {
            access == Access::Publish && stream == "real"
        }
        fn is_key(&self, s: &str) -> bool {
            s == "k3y"
        }
    }

    /// Refuses anything without a token; a keyed request passes.
    struct NeedsToken;

    impl Gate for NeedsToken {
        fn check<'a>(
            &'a self,
            _access: Access,
            _stream: &'a str,
            token: Option<&'a str>,
            _ip: Option<std::net::IpAddr>,
        ) -> crate::gate::GateFuture<'a> {
            Box::pin(async move { if token == Some("jwt") { Ok(()) } else { Err(Denied::Missing) } })
        }
        fn check_keyed<'a>(
            &'a self,
            _access: Access,
            _stream: &'a str,
            _ip: Option<std::net::IpAddr>,
        ) -> crate::gate::GateFuture<'a> {
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn stream_keys_resolve_before_the_rewrite_and_stand_in_for_the_token() {
        let reg = Registry::new();
        reg.set_gate(Arc::new(NeedsToken));
        reg.set_rewrite(Arc::new(Router));
        reg.set_stream_keys(Arc::new(Keys));
        assert_eq!(reg.resolve(Access::Publish, "rtmp", "k3y", None, None).await, Ok("real".into()));
        assert_eq!(reg.resolve(Access::Publish, "whip", "real", Some("k3y"), None).await, Ok("real".into()));
        // A key is only good for its own stream in the token slot.
        assert_eq!(reg.resolve(Access::Publish, "whip", "cam", Some("k3y"), None).await, Err(Denied::Missing));
        // Publish-only key: a play of the key is just an unknown name.
        assert_eq!(reg.resolve(Access::Play, "hls", "k3y", Some("jwt"), None).await, Ok("k3y".into()));
        // `real` takes its key only, even with a good token or through a rewrite.
        assert!(matches!(
            reg.resolve(Access::Publish, "rtmp", "real", Some("jwt"), None).await,
            Err(Denied::Refused(_))
        ));
        assert!(matches!(
            reg.resolve(Access::Publish, "rtmp", "alias", Some("jwt"), None).await,
            Err(Denied::Refused(_))
        ));
        assert_eq!(reg.resolve(Access::Publish, "rtmp", "cam", Some("jwt"), None).await, Ok("cam".into()));
        assert_eq!(reg.loggable("k3y"), "****");
        assert_eq!(reg.loggable("cam"), "cam");
        reg.clear_stream_keys();
        assert_eq!(reg.loggable("k3y"), "k3y");
        assert_eq!(reg.resolve(Access::Publish, "rtmp", "k3y", None, None).await, Err(Denied::Missing));
    }
}
