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

#[derive(Debug, Clone)]
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

/// Running channels. Cheap to clone. Dropping it does not stop them; they
/// run as long as the runtime does (see [`ChannelHandle::stop`]).
#[derive(Clone)]
pub struct ChannelHandle {
    inner: Arc<Inner>,
}

struct Inner {
    registry: Arc<Registry>,
    channels: Vec<Arc<Shared>>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl ChannelHandle {
    /// Every channel's status, in configuration order.
    pub fn status(&self) -> Vec<ChannelStatus> {
        self.inner.channels.iter().map(|c| c.status.lock().clone()).collect()
    }

    /// Jumps channel `name` to its next item at once. False if unknown.
    pub fn skip(&self, name: &str) -> bool {
        match self.find(name) {
            Some(c) => {
                c.skip.send_modify(|v| *v = v.wrapping_add(1));
                true
            }
            None => false,
        }
    }

    /// Stops every channel task; their streams end.
    pub fn stop(&self) {
        for t in self.inner.tasks.lock().drain(..) {
            t.abort();
        }
    }

    pub(crate) fn find(&self, name: &str) -> Option<&Arc<Shared>> {
        self.inner.channels.iter().find(|c| c.status.lock().name == name)
    }

    pub(crate) fn registry(&self) -> &Arc<Registry> {
        &self.inner.registry
    }
}

/// Spawns one task per channel on the current tokio runtime.
pub fn start(registry: Arc<Registry>, cfg: ChannelConfig) -> ChannelHandle {
    let mut channels = Vec::new();
    let mut tasks = Vec::new();
    for ch in cfg.channels {
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
        channels.push(shared.clone());
        tasks.push(tokio::spawn(player::run(ch, cfg.buffer, registry.clone(), shared)));
    }
    ChannelHandle { inner: Arc::new(Inner { registry, channels, tasks: Mutex::new(tasks) }) }
}

/// The channel API routes, with their state already applied.
pub fn router(handle: ChannelHandle) -> axum::Router {
    http::router(handle)
}
