//! 24/7 linear channels from files (milestone M13).
//!
//! A channel is a playlist of media files published into the
//! [`Registry`] as one continuous live stream, so every output (LL-HLS,
//! WHEP, MoQ, SRT, recording) serves it unchanged. Entry points are fixed
//! by the orchestrator: [`start`] spawns one task per channel and
//! [`router`] exposes its status and a skip control.
//!
//! Behaviour, in short (details in `player.rs`):
//!
//! - **Items** are files or directories (a directory is its media files
//!   sorted by name, not recursive), re-listed on every pass so files that
//!   appear later are picked up. Containers: progressive MP4/MOV and
//!   MPEG-TS. Codecs: H.264, H.265, AAC, Opus (Opus in MP4 only).
//! - **Pacing** is real time against a wall clock, like a live encoder,
//!   with a lead of [`LEAD`] so outputs are never starved.
//! - **Timestamp stitching**: each file starts at its first keyframe and
//!   continues where the previous one ended, on every track, across files
//!   and loops; timestamps are strictly monotonic per track.
//! - **Tracks** are fixed by the first playable file. A later file whose
//!   tracks differ (codec, resolution, sample rate, channels, codec
//!   config) is skipped with an error for that item. Normalizing such files
//!   through `caudal-transcode` is a later step.
//! - With nothing playable, the channel stays idle and retries every
//!   [`RETRY`]; it never busy-loops. Missing or corrupt files, a skip, or a
//!   stream name already in use are logged and never panic.
//! - `loop = false` ends the stream (drops the publisher) after the last
//!   item; `loop = true` starts over, reshuffling each pass if `shuffle`.
//!
//! Routes (absolute, merged at the root):
//! - `GET /api/v1/channels` → JSON array of channel status
//! - `POST /api/v1/channels/{name}/skip` → 204, or 404 for an unknown name.
//!   Asks `Registry::authorize(Publish, name)` (`?token=` or Bearer).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use caudal_core::{BufferConfig, Registry};
use parking_lot::Mutex;
use serde::Serialize;
use tokio::sync::watch;

mod http;
mod player;
mod source;

/// How far ahead of the wall clock frames are pushed.
pub const LEAD: Duration = Duration::from_millis(100);
/// How long an idle channel (nothing playable, or its name taken) waits
/// before trying again.
pub const RETRY: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Default)]
pub struct ChannelConfig {
    pub channels: Vec<Channel>,
    pub buffer: BufferConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Channel {
    /// Stream name the channel publishes as.
    pub name: String,
    /// Files and directories, played in order.
    pub items: Vec<PathBuf>,
    /// Start over after the last item (config default: true).
    pub r#loop: bool,
    /// Play the items in a new random order on every pass (default: false).
    pub shuffle: bool,
}

/// Status of one channel, as served by `GET /api/v1/channels`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChannelStatus {
    pub name: String,
    pub state: ChannelState,
    /// Position of the current (or last) item in this pass's playlist.
    pub index: usize,
    /// Number of items in this pass's playlist, directories expanded.
    pub items: usize,
    pub now_playing: Option<NowPlaying>,
    /// The most recent error, cleared after a pass with no errors.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChannelState {
    Playing,
    Idle,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NowPlaying {
    pub path: String,
    pub position_secs: f64,
    /// `None` when the container does not say (estimated for TS).
    pub duration_secs: Option<f64>,
}

pub(crate) struct Shared {
    pub(crate) status: Mutex<ChannelStatus>,
    /// Bumped by every skip request.
    pub(crate) skip: watch::Sender<u64>,
}

struct Entry {
    cfg: Channel,
    shared: Arc<Shared>,
    task: tokio::task::JoinHandle<()>,
}

/// Running channels. Cheap to clone. Dropping it does not stop them; they
/// run as long as the runtime does (see [`ChannelHandle::stop`]).
#[derive(Clone)]
pub struct ChannelHandle {
    inner: Arc<Inner>,
}

struct Inner {
    registry: Arc<Registry>,
    buffer: BufferConfig,
    entries: Mutex<Vec<Entry>>,
}

fn spawn_channel(registry: &Arc<Registry>, buffer: BufferConfig, ch: Channel) -> Entry {
    let shared = Arc::new(Shared {
        status: Mutex::new(ChannelStatus {
            name: ch.name.clone(),
            state: ChannelState::Idle,
            index: 0,
            items: 0,
            now_playing: None,
            error: None,
        }),
        skip: watch::channel(0).0,
    });
    let cfg = ch.clone();
    let task = tokio::spawn(player::run(ch, buffer, registry.clone(), shared.clone()));
    Entry { cfg, shared, task }
}

impl ChannelHandle {
    /// Every channel's status, in configuration order.
    pub fn status(&self) -> Vec<ChannelStatus> {
        self.inner.entries.lock().iter().map(|e| e.shared.status.lock().clone()).collect()
    }

    /// Jumps channel `name` to its next item at once. False if unknown.
    pub fn skip(&self, name: &str) -> bool {
        match self.find(name) {
            Some(shared) => {
                shared.skip.send_modify(|v| *v = v.wrapping_add(1));
                true
            }
            None => false,
        }
    }

    /// Stops every channel task; their streams end.
    pub fn stop(&self) {
        for e in self.inner.entries.lock().drain(..) {
            e.task.abort();
        }
    }

    /// Applies a new channel list: a channel whose name, items, `loop` and
    /// `shuffle` are all unchanged is left running untouched (its status
    /// and playback position survive the reload); a channel that changed,
    /// or one that's new, is (re)started; a channel dropped from the list
    /// is stopped, ending its stream. Never touches any other channel.
    pub fn reload(&self, cfg: ChannelConfig) {
        let mut entries = self.inner.entries.lock();
        let mut remaining = std::mem::take(&mut *entries);
        let mut next = Vec::with_capacity(cfg.channels.len());
        for ch in cfg.channels {
            if let Some(pos) = remaining.iter().position(|e| e.cfg == ch) {
                next.push(remaining.remove(pos));
                continue;
            }
            if let Some(pos) = remaining.iter().position(|e| e.cfg.name == ch.name) {
                remaining.remove(pos).task.abort();
            }
            next.push(spawn_channel(&self.inner.registry, self.inner.buffer, ch));
        }
        for gone in remaining {
            gone.task.abort();
        }
        *entries = next;
    }

    pub(crate) fn find(&self, name: &str) -> Option<Arc<Shared>> {
        self.inner.entries.lock().iter().find(|e| e.cfg.name == name).map(|e| e.shared.clone())
    }

    pub(crate) fn registry(&self) -> &Arc<Registry> {
        &self.inner.registry
    }
}

/// Spawns one task per channel on the current tokio runtime.
pub fn start(registry: Arc<Registry>, cfg: ChannelConfig) -> ChannelHandle {
    let entries = cfg.channels.into_iter().map(|ch| spawn_channel(&registry, cfg.buffer, ch)).collect();
    ChannelHandle { inner: Arc::new(Inner { registry, buffer: cfg.buffer, entries: Mutex::new(entries) }) }
}

/// The channel API routes, with their state already applied.
pub fn router(handle: ChannelHandle) -> axum::Router {
    http::router(handle)
}

#[cfg(test)]
mod reload_tests {
    use super::*;

    fn ch(name: &str, r#loop: bool) -> Channel {
        Channel { name: name.into(), items: Vec::new(), r#loop, shuffle: false }
    }

    fn task_ids(handle: &ChannelHandle) -> Vec<tokio::task::Id> {
        handle.inner.entries.lock().iter().map(|e| e.task.id()).collect()
    }

    #[tokio::test]
    async fn unchanged_channel_keeps_its_task() {
        let registry = Registry::new();
        let handle =
            start(registry, ChannelConfig { channels: vec![ch("a", true)], buffer: BufferConfig::default() });
        let before = task_ids(&handle);
        handle.reload(ChannelConfig { channels: vec![ch("a", true)], buffer: BufferConfig::default() });
        assert_eq!(task_ids(&handle), before, "unchanged channel must not be restarted");
    }

    #[tokio::test]
    async fn changed_removed_and_added_channels() {
        let registry = Registry::new();
        let handle = start(
            registry,
            ChannelConfig { channels: vec![ch("a", true), ch("b", true)], buffer: BufferConfig::default() },
        );
        let before = task_ids(&handle);

        // `a`'s `loop` flag changes (restart), `b` is dropped (stop), `c`
        // is new (start).
        handle.reload(ChannelConfig {
            channels: vec![ch("a", false), ch("c", true)],
            buffer: BufferConfig::default(),
        });

        let names: Vec<String> = handle.status().iter().map(|s| s.name.clone()).collect();
        assert!(names.contains(&"a".to_string()));
        assert!(names.contains(&"c".to_string()));
        assert!(!names.contains(&"b".to_string()), "removed channel is gone");
        let a_task = handle.inner.entries.lock().iter().find(|e| e.cfg.name == "a").unwrap().task.id();
        assert!(!before.contains(&a_task), "changed channel got a new task");
    }
}
