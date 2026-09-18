//! Stream names to live streams, with one publisher per name.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::media::{Frame, TrackInfo, valid_stream_name};
use crate::stream::{BufferConfig, PushError, StartAt, Stream, Subscriber};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PublishError {
    #[error("invalid stream name")]
    InvalidName,
    #[error("stream {0} already has a publisher")]
    Busy(String),
}

#[derive(Default)]
pub struct Registry {
    streams: Mutex<HashMap<Arc<str>, Arc<Stream>>>,
}

impl Registry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
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
}

impl Drop for Publisher {
    fn drop(&mut self) {
        self.stream.end();
        let mut map = self.registry.streams.lock();
        if map.get(self.stream.name()).is_some_and(|s| Arc::ptr_eq(s, &self.stream)) {
            map.remove(self.stream.name());
        }
        tracing::info!(stream = %self.stream.name(), "publish ended");
    }
}
