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

use std::path::PathBuf;
use std::sync::Arc;

use caudal_core::Registry;

#[derive(Debug, Clone)]
pub struct RecordConfig {
    pub dir: PathBuf,
    /// Stream names or `prefix*` patterns to record; `["*"]` = everything.
    pub streams: Vec<String>,
    /// Target segment length; segments are cut on the first keyframe after it.
    pub segment_secs: u32,
    /// Delete recordings older than this; `None` keeps everything.
    pub retention_hours: Option<u32>,
    /// Also upload each closed segment and playlist to object storage,
    /// e.g. `s3://bucket/prefix` (credentials from the environment).
    pub upload_url: Option<String>,
}

pub struct RecordService {
    _private: (),
}

impl RecordService {
    pub fn router(&self) -> axum::Router {
        axum::Router::new()
    }
}

/// Starts recording matching streams (current and future). Must be called
/// inside a tokio runtime.
pub fn start(registry: Arc<Registry>, cfg: RecordConfig) -> std::io::Result<RecordService> {
    let _ = (registry, cfg);
    Err(std::io::Error::other("recording not implemented yet"))
}
