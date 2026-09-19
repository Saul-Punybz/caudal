//! Recording, VOD and clips. Entry points fixed by the orchestrator.
//!
//! A recording is one publish of one stream, written as CMAF: `init.mp4`
//! plus numbered `.m4s` segments cut on keyframes, and a VOD playlist that
//! grows while live and gets `#EXT-X-ENDLIST` when the publish ends. Crash
//! safe by construction: every closed segment is a complete file.
//!
//! Layout: `<dir>/<stream>/<id>/{init.mp4, seg-000001.m4s, ..., index.m3u8, meta.json}`
//! where `<id>` is the UTC start time, `YYYYMMDDTHHMMSSZ`.
//!
//! Routes (absolute, merged at the root):
//! - `GET /api/v1/recordings` → JSON list, newest first
//! - `GET /api/v1/recordings/{stream}/{id}` → one recording's metadata
//! - `DELETE /api/v1/recordings/{stream}/{id}`
//! - `GET /vod/{stream}/{id}/index.m3u8` and `/vod/{stream}/{id}/{file}`
//! - `POST /api/v1/clips` with JSON `{"stream": "...", "id": "...", "from_ms": 0, "to_ms": 30000}`
//!   → a progressive MP4 of that range (download), cut on keyframes
//!
//! The recorder reads each stream with `StartAt::Oldest`: it subscribes when
//! the publish is announced, so the ring holds at most the first frames, and
//! starting at the oldest keyframe guarantees the recording begins at the
//! publish's first keyframe even if the task is scheduled late (with
//! `LiveEdge` a late start would skip whole GOPs). For a stream already live
//! when recording starts, it also keeps the buffered DVR window.
//!
//! Every route asks `Registry::authorize` (`?token=` or Bearer): `Play` to
//! list, read, watch or clip, `Publish` to delete.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use caudal_core::{Registry, StartAt, Stream};
use parking_lot::Mutex;
use tokio::sync::broadcast;

mod clip;
mod http;
mod meta;
mod recorder;
mod schedule;
mod segmenter;
mod upload;

pub use schedule::{ScheduleConfig, validate_schedules};

#[derive(Debug, Clone)]
pub struct RecordConfig {
    pub dir: PathBuf,
    /// Stream names or `prefix*` patterns to record always, regardless of
    /// `schedules`; `["*"]` = everything.
    pub streams: Vec<String>,
    /// Target segment length; segments are cut on the first keyframe after it.
    pub segment_secs: u32,
    /// Delete recordings older than this; `None` keeps everything.
    pub retention_hours: Option<u32>,
    /// Also upload each closed segment and playlist to object storage,
    /// e.g. `s3://bucket/prefix` (credentials from the environment).
    pub upload_url: Option<String>,
    /// `[[record.schedule]]`: start/stop windows for streams not already
    /// covered by `streams`. Validate with [`validate_schedules`] before
    /// passing here (`start` does this).
    pub schedules: Vec<ScheduleConfig>,
}

/// How often the retention sweep runs.
const SWEEP_EVERY: Duration = Duration::from_secs(600);

pub(crate) struct Shared {
    pub registry: Arc<Registry>,
    pub cfg: RecordConfig,
    /// `[server] trusted_proxies`, for resolving `X-Forwarded-For` on this
    /// crate's HTTP routes; see `caudal_core::net::resolve_forwarded`.
    pub trusted_proxies: Vec<caudal_core::Cidr>,
    /// `(stream, id)` of recordings being written right now.
    pub active: Mutex<HashSet<(String, String)>>,
    /// One recorder task per live stream.
    pub recorders: Mutex<HashMap<String, Arc<Stream>>>,
    pub uploader: Option<Arc<upload::Uploader>>,
    pub scheduler: schedule::Scheduler,
}

pub struct RecordService {
    shared: Arc<Shared>,
}

impl RecordService {
    pub fn router(&self) -> axum::Router {
        http::router(self.shared.clone())
    }

    /// Runs the retention sweep now (it also runs every 10 minutes).
    /// Returns how many recordings were deleted.
    #[doc(hidden)]
    pub async fn sweep(&self) -> usize {
        sweep(&self.shared).await
    }

    /// Uploads queued or in flight.
    #[doc(hidden)]
    pub fn uploads_pending(&self) -> usize {
        self.shared.uploader.as_ref().map_or(0, |u| u.pending())
    }
}

/// Starts recording matching streams (current and future). Must be called
/// inside a tokio runtime. `trusted_proxies`: see [`Shared::trusted_proxies`].
pub fn start(
    registry: Arc<Registry>,
    cfg: RecordConfig,
    trusted_proxies: Vec<caudal_core::Cidr>,
) -> std::io::Result<RecordService> {
    std::fs::create_dir_all(&cfg.dir)?;
    // Fail now, not on the first publish, if the directory is not writable.
    let probe = cfg.dir.join(".caudal-write-test");
    std::fs::write(&probe, b"")?;
    let _ = std::fs::remove_file(&probe);
    let rt = tokio::runtime::Handle::try_current().map_err(std::io::Error::other)?;

    recover(&cfg.dir);
    let uploader = cfg.upload_url.as_deref().and_then(|url| match upload::Uploader::start(url) {
        Ok(u) => Some(u),
        Err(e) => {
            // Recording locally matters more than mirroring it.
            tracing::error!(error = %e, "upload disabled; recording locally only");
            None
        }
    });
    let scheduler = schedule::Scheduler::start(&cfg.schedules);
    let shared = Arc::new(Shared {
        registry: registry.clone(),
        cfg,
        trusted_proxies,
        active: Mutex::default(),
        recorders: Mutex::default(),
        uploader,
        scheduler,
    });

    // Subscribe before listing, so a publish in between is seen at least
    // once (`spawn_recorder` ignores duplicates).
    let publishes = registry.subscribe_publishes();
    for stream in registry.list() {
        spawn_recorder(&shared, &rt, stream);
    }
    rt.spawn(listen(shared.clone(), rt.clone(), publishes));
    if shared.cfg.retention_hours.is_some() {
        let s = shared.clone();
        rt.spawn(async move {
            let mut tick = tokio::time::interval(SWEEP_EVERY);
            loop {
                tick.tick().await;
                sweep(&s).await;
            }
        });
    }
    Ok(RecordService { shared })
}

pub(crate) fn matches(patterns: &[String], name: &str) -> bool {
    patterns.iter().any(|p| match p.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => p == name,
    })
}

fn spawn_recorder(shared: &Arc<Shared>, rt: &tokio::runtime::Handle, stream: Arc<Stream>) {
    // `[record] streams` always wins: a stream listed there records
    // continuously even if a schedule also names it.
    let always = matches(&shared.cfg.streams, stream.name());
    let gate = if always { None } else { shared.scheduler.gate_for(stream.name()) };
    if !always && gate.is_none() {
        return;
    }
    {
        let mut map = shared.recorders.lock();
        if map.get(stream.name()).is_some_and(|s| Arc::ptr_eq(s, &stream)) {
            return;
        }
        map.insert(stream.name().to_owned(), stream.clone());
    }
    // Subscribe now, so no frame pushed before the task runs is missed.
    // A recorder is not a viewer.
    let sub = stream.subscribe_internal(StartAt::Oldest);
    tracing::debug!(stream = %stream.name(), scheduled = !always, "recorder started");
    rt.spawn(recorder::run(shared.clone(), stream, sub, gate));
}

async fn listen(shared: Arc<Shared>, rt: tokio::runtime::Handle, mut publishes: broadcast::Receiver<Arc<Stream>>) {
    loop {
        match publishes.recv().await {
            Ok(stream) => spawn_recorder(&shared, &rt, stream),
            Err(broadcast::error::RecvError::Lagged(_)) => {
                for stream in shared.registry.list() {
                    spawn_recorder(&shared, &rt, stream);
                }
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// `(stream, id, dir)` of every recording directory on disk.
pub(crate) fn recording_dirs(root: &Path) -> Vec<(String, String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(streams) = std::fs::read_dir(root) else { return out };
    for s in streams.flatten() {
        let Ok(stream) = s.file_name().into_string() else { continue };
        if !caudal_core::media::valid_stream_name(&stream) || !s.path().is_dir() {
            continue;
        }
        let Ok(ids) = std::fs::read_dir(s.path()) else { continue };
        for i in ids.flatten() {
            let Ok(id) = i.file_name().into_string() else { continue };
            if meta::valid_id(&id) && i.path().is_dir() {
                out.push((stream.clone(), id, i.path()));
            }
        }
    }
    out
}

/// After a crash: drop half-written files and close recordings that were
/// still open (VOD playlist, `ended_at` from the last write).
fn recover(root: &Path) {
    for (stream, id, dir) in recording_dirs(root) {
        if let Ok(files) = std::fs::read_dir(&dir) {
            for f in files.flatten() {
                if f.file_name().to_string_lossy().ends_with(".tmp") {
                    let _ = std::fs::remove_file(f.path());
                }
            }
        }
        let meta_path = dir.join("meta.json");
        let Some(mut m) = std::fs::read(&meta_path).ok().and_then(|b| serde_json::from_slice::<meta::Meta>(&b).ok())
        else {
            continue;
        };
        if m.ended_at.is_some() {
            continue;
        }
        let last = std::fs::metadata(&meta_path).and_then(|md| md.modified()).unwrap_or_else(|_| SystemTime::now());
        m.ended_at = Some(meta::rfc3339(last));
        m.error.get_or_insert_with(|| "interrupted: the server stopped while recording".into());
        if let Ok(pl) = std::fs::read_to_string(dir.join("index.m3u8")) {
            let _ = std::fs::write(dir.join("index.m3u8"), meta::close_playlist(&pl));
        }
        if let Ok(json) = serde_json::to_vec_pretty(&m) {
            let _ = std::fs::write(&meta_path, json);
        }
        tracing::warn!(%stream, %id, "closed a recording interrupted by a restart");
    }
}

/// Deletes ended recordings whose `ended_at` is past the retention window.
async fn sweep(shared: &Arc<Shared>) -> usize {
    let Some(hours) = shared.cfg.retention_hours else { return 0 };
    let cutoff = SystemTime::now() - Duration::from_secs(u64::from(hours) * 3600);
    let root = shared.cfg.dir.clone();
    let dirs = tokio::task::spawn_blocking(move || recording_dirs(&root)).await.unwrap_or_default();
    let mut deleted = 0;
    for (stream, id, dir) in dirs {
        if shared.active.lock().contains(&(stream.clone(), id.clone())) {
            continue;
        }
        let Some(m) = meta::read_meta(&dir).await else { continue };
        let Some(ended) = m.ended_at.as_deref().and_then(meta::parse_rfc3339) else { continue };
        if ended >= cutoff {
            continue;
        }
        match tokio::fs::remove_dir_all(&dir).await {
            Ok(()) => {
                tracing::info!(%stream, %id, "recording deleted by retention");
                deleted += 1;
            }
            Err(e) => tracing::warn!(%stream, %id, error = %e, "retention could not delete recording"),
        }
    }
    deleted
}

#[cfg(test)]
mod tests {
    #[test]
    fn stream_patterns() {
        let p = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(super::matches(&p(&["*"]), "anything"));
        assert!(super::matches(&p(&["cam*"]), "cam1"));
        assert!(!super::matches(&p(&["cam*"]), "live"));
        assert!(super::matches(&p(&["live", "x"]), "live"));
        assert!(!super::matches(&p(&["live"]), "live2"));
        assert!(!super::matches(&p(&[]), "live"));
    }
}
