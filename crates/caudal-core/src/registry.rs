//! Stream names to live streams, with one publisher per name.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use tokio::sync::broadcast;

use crate::gate::{Access, Denied, Gate};
use crate::media::{Cue, Frame, TrackInfo, valid_stream_name};
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
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            streams: Mutex::default(),
            published: broadcast::channel(64).0,
            ended: broadcast::channel(64).0,
            gate: RwLock::new(None),
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

    /// Asks the installed gate. Ingest calls this before `publish`, outputs
    /// before serving a viewer. `ip` is the caller's resolved address, when
    /// the protocol has one to give (see [`Gate::check`]).
    pub async fn authorize(
        &self,
        access: Access,
        stream: &str,
        token: Option<&str>,
        ip: Option<std::net::IpAddr>,
    ) -> Result<(), Denied> {
        let gate = self.gate.read().clone();
        match gate {
            Some(g) => g.check(access, stream, token, ip).await,
            None => Ok(()),
        }
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
