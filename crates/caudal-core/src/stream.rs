//! The live buffer between one publisher and many viewers.
//!
//! MistServer keeps this buffer in shared-memory pages and runs every input
//! and output as its own process. Here it is a single in-process ring:
//!
//! - The publisher never waits on a viewer. A viewer that falls off the back
//!   of the ring is moved forward to the next keyframe and told how much it
//!   skipped, so it can never stall ingest or grow memory.
//! - Memory is bounded twice: by time (`window`) and by bytes (`max_bytes`).
//!   Eviction drops whole GOPs, so every frame still in the ring can be
//!   decoded from a keyframe that is also still in the ring.
//! - Viewers join on a keyframe, so the first thing they receive decodes.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use parking_lot::RwLock;
use tokio::sync::watch;

use crate::media::{Frame, TrackId, TrackInfo, TrackKind};

#[derive(Debug, Clone, Copy)]
pub struct BufferConfig {
    /// How much history to keep. MistServer's default is 50 s.
    pub window: Duration,
    /// Hard cap on buffered payload bytes, whatever the window says.
    pub max_bytes: usize,
}

impl Default for BufferConfig {
    fn default() -> Self {
        Self { window: Duration::from_secs(50), max_bytes: 256 * 1024 * 1024 }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PushError {
    #[error("frame references track {0:?}, which was never announced")]
    UnknownTrack(TrackId),
    #[error("the stream has ended")]
    Ended,
}

/// Where a new viewer starts reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartAt {
    /// The most recent keyframe: lowest latency.
    LiveEdge,
    /// The oldest keyframe still buffered: the whole DVR window.
    Oldest,
}

/// What a viewer receives next.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Frame(Arc<Frame>),
    /// The track list changed; read it again with `Subscriber::tracks`
    /// before handling further frames. Always the first event a viewer gets.
    TracksChanged,
    /// The viewer was too slow and was moved forward to a keyframe.
    Lagged {
        skipped: u64,
    },
    /// The publisher is gone and every buffered frame has been delivered.
    End,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StreamStats {
    pub frames_buffered: usize,
    pub bytes_buffered: usize,
    /// Buffered span in microseconds, oldest to newest frame.
    pub buffered_micros: i64,
    pub frames_in: u64,
    pub bytes_in: u64,
    pub viewers: usize,
}

struct Entry {
    frame: Arc<Frame>,
    micros: i64,
}

struct Ring {
    frames: VecDeque<Entry>,
    /// Sequence number of `frames[0]`. Sequence numbers never repeat.
    base: u64,
    /// Sequence numbers of join points, oldest first.
    keys: VecDeque<u64>,
    bytes: usize,
    newest_micros: i64,
    tracks: Vec<TrackInfo>,
    tracks_version: u64,
    has_video: bool,
    ended: bool,
}

impl Ring {
    fn end_seq(&self) -> u64 {
        self.base + self.frames.len() as u64
    }

    fn micros_at(&self, seq: u64) -> i64 {
        self.frames[(seq - self.base) as usize].micros
    }

    /// Drops everything before `seq`.
    fn evict_to(&mut self, seq: u64) {
        while self.base < seq {
            let e = self.frames.pop_front().expect("evicting past the end");
            self.bytes -= e.frame.data.len();
            self.base += 1;
        }
        while self.keys.front().is_some_and(|&k| k < self.base) {
            self.keys.pop_front();
        }
    }

    fn evict(&mut self, cfg: &BufferConfig) {
        // Frames before the first keyframe can never be decoded by anyone
        // who joins now; nobody can have joined before it either.
        if let Some(&first) = self.keys.front() {
            self.evict_to(first);
        }
        let cutoff = self.newest_micros - cfg.window.as_micros() as i64;
        while self.keys.len() >= 2 {
            let second = self.keys[1];
            let over_bytes = self.bytes > cfg.max_bytes;
            if over_bytes || self.micros_at(second) <= cutoff {
                self.evict_to(second);
            } else {
                break;
            }
        }
        // No usable keyframe structure (a video source that has not sent a
        // keyframe yet): the byte cap still holds.
        if self.keys.len() < 2 {
            while self.bytes > cfg.max_bytes && self.frames.len() > 1 {
                let next = self.base + 1;
                self.evict_to(next);
            }
        }
    }
}

/// A live stream. Created by [`crate::Registry::publish`].
pub struct Stream {
    name: Arc<str>,
    cfg: BufferConfig,
    ring: RwLock<Ring>,
    /// Bumped on every change; viewers wait on it.
    notify: watch::Sender<u64>,
    frames_in: AtomicU64,
    bytes_in: AtomicU64,
    viewers: AtomicUsize,
}

impl Stream {
    pub(crate) fn new(name: Arc<str>, cfg: BufferConfig) -> Arc<Self> {
        Arc::new(Self {
            name,
            cfg,
            ring: RwLock::new(Ring {
                frames: VecDeque::new(),
                base: 0,
                keys: VecDeque::new(),
                bytes: 0,
                newest_micros: i64::MIN,
                tracks: Vec::new(),
                tracks_version: 0,
                has_video: false,
                ended: false,
            }),
            notify: watch::channel(0).0,
            frames_in: AtomicU64::new(0),
            bytes_in: AtomicU64::new(0),
            viewers: AtomicUsize::new(0),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn tracks(&self) -> Vec<TrackInfo> {
        self.ring.read().tracks.clone()
    }

    pub fn is_ended(&self) -> bool {
        self.ring.read().ended
    }

    pub fn stats(&self) -> StreamStats {
        let r = self.ring.read();
        let span = match (r.frames.front(), r.frames.back()) {
            (Some(a), Some(b)) => b.micros - a.micros,
            _ => 0,
        };
        StreamStats {
            frames_buffered: r.frames.len(),
            bytes_buffered: r.bytes,
            buffered_micros: span,
            frames_in: self.frames_in.load(Ordering::Relaxed),
            bytes_in: self.bytes_in.load(Ordering::Relaxed),
            viewers: self.viewers.load(Ordering::Relaxed),
        }
    }

    fn wake(&self) {
        self.notify.send_modify(|v| *v = v.wrapping_add(1));
    }

    pub(crate) fn set_tracks(&self, tracks: Vec<TrackInfo>) -> Result<(), PushError> {
        {
            let mut r = self.ring.write();
            if r.ended {
                return Err(PushError::Ended);
            }
            if r.tracks == tracks {
                return Ok(());
            }
            r.has_video = tracks.iter().any(|t| t.kind() == TrackKind::Video);
            r.tracks = tracks;
            r.tracks_version += 1;
        }
        self.wake();
        Ok(())
    }

    pub(crate) fn push(&self, frame: Frame) -> Result<(), PushError> {
        let len = frame.data.len();
        {
            let mut r = self.ring.write();
            if r.ended {
                return Err(PushError::Ended);
            }
            let track = r.tracks.iter().find(|t| t.id == frame.track).ok_or(PushError::UnknownTrack(frame.track))?;
            let micros = track.to_micros(frame.dts);
            // A join point is a video keyframe, or any audio frame in an
            // audio-only stream.
            let sync = frame.keyframe && (track.kind() == TrackKind::Video || !r.has_video);
            let seq = r.end_seq();
            if sync {
                r.keys.push_back(seq);
            }
            r.newest_micros = r.newest_micros.max(micros);
            r.bytes += len;
            r.frames.push_back(Entry { frame: Arc::new(frame), micros });
            r.evict(&self.cfg);
        }
        self.frames_in.fetch_add(1, Ordering::Relaxed);
        self.bytes_in.fetch_add(len as u64, Ordering::Relaxed);
        self.wake();
        Ok(())
    }

    pub(crate) fn end(&self) {
        self.ring.write().ended = true;
        self.wake();
    }

    pub fn subscribe(self: &Arc<Self>, start: StartAt) -> Subscriber {
        let next = {
            let r = self.ring.read();
            match start {
                StartAt::LiveEdge => r.keys.back().copied(),
                StartAt::Oldest => r.keys.front().copied(),
            }
            // No keyframe yet: wait for the first one.
            .unwrap_or(u64::MAX)
        };
        self.viewers.fetch_add(1, Ordering::Relaxed);
        Subscriber { stream: self.clone(), next, tracks_version: 0, notify: self.notify.subscribe() }
    }
}

/// One viewer's read position in a [`Stream`].
pub struct Subscriber {
    stream: Arc<Stream>,
    /// Next sequence number to deliver. `u64::MAX` means "the first
    /// keyframe that arrives".
    next: u64,
    tracks_version: u64,
    notify: watch::Receiver<u64>,
}

impl Subscriber {
    pub fn stream(&self) -> &Arc<Stream> {
        &self.stream
    }

    pub fn tracks(&self) -> Vec<TrackInfo> {
        self.stream.tracks()
    }

    /// Returns the next event without waiting, or `None` if the viewer is
    /// caught up with a live stream.
    pub fn try_recv(&mut self) -> Option<Event> {
        let r = self.stream.ring.read();
        if r.tracks_version != self.tracks_version {
            self.tracks_version = r.tracks_version;
            return Some(Event::TracksChanged);
        }
        if self.next == u64::MAX {
            match r.keys.front() {
                Some(&k) => self.next = k,
                None if r.ended => return Some(Event::End),
                None => return None,
            }
        }
        if self.next < r.base {
            // Fell off the back of the ring: resume at the next join point.
            let resume = r.keys.iter().copied().find(|&k| k >= r.base).unwrap_or(r.end_seq());
            let skipped = resume - self.next;
            self.next = if resume == r.end_seq() && r.keys.is_empty() { u64::MAX } else { resume };
            return Some(Event::Lagged { skipped });
        }
        if self.next < r.end_seq() {
            let frame = r.frames[(self.next - r.base) as usize].frame.clone();
            self.next += 1;
            return Some(Event::Frame(frame));
        }
        r.ended.then_some(Event::End)
    }

    /// Waits for the next event. Cancel-safe.
    pub async fn recv(&mut self) -> Event {
        loop {
            // Mark the current version seen *before* looking at the ring, so
            // a push that lands after the look still wakes us.
            self.notify.borrow_and_update();
            if let Some(ev) = self.try_recv() {
                return ev;
            }
            if self.notify.changed().await.is_err() {
                return Event::End;
            }
        }
    }

    /// Jumps to the newest keyframe, dropping whatever was pending. For
    /// outputs that prefer staying live over staying complete.
    pub fn skip_to_live(&mut self) -> u64 {
        let r = self.stream.ring.read();
        match r.keys.back() {
            Some(&k) if k > self.next => {
                let skipped = k - self.next;
                self.next = k;
                skipped
            }
            _ => 0,
        }
    }

    /// Frames between this viewer and the newest frame.
    pub fn backlog(&self) -> u64 {
        let r = self.stream.ring.read();
        r.end_seq().saturating_sub(self.next.max(r.base))
    }
}

impl Drop for Subscriber {
    fn drop(&mut self) {
        self.stream.viewers.fetch_sub(1, Ordering::Relaxed);
    }
}
