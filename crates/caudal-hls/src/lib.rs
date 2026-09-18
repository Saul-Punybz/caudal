//! LL-HLS / CMAF output and the `/play/{name}` page. Entry point fixed by
//! the orchestrator.

use std::sync::Arc;

use caudal_core::Registry;

#[derive(Debug, Clone, Copy)]
pub struct HlsConfig {
    /// Target partial-segment duration.
    pub part_ms: u32,
    /// Target full-segment duration.
    pub segment_ms: u32,
}

/// Serves `/hls/{name}/index.m3u8`, `/hls/{name}/init.mp4`,
/// `/hls/{name}/{segment}.m4s` and `/play/{name}`. Mounted at the root by
/// the server; paths are absolute.
pub fn router(registry: Arc<Registry>, cfg: HlsConfig) -> axum::Router {
    let _ = (registry, cfg);
    todo!("agent C")
}
