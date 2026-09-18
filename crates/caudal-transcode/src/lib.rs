//! Transcoding: each matching source stream `<name>` gets renditions
//! published as streams `<name>+<label>` (e.g. `main+480p`), which the
//! LL-HLS output groups into one multivariant playlist. Entry point fixed by
//! the orchestrator.
//!
//! Engines: `Ffmpeg` runs ffmpeg as an external process (never linked, so no
//! GPL in the binary), fed MPEG-TS on stdin and read as MPEG-TS on stdout via
//! `caudal-ts`; `RustyH264` encodes in-process with the pure-Rust
//! `rusty_h264` (video only; audio still needs ffmpeg or passthrough).

use std::path::PathBuf;
use std::sync::Arc;

use caudal_core::Registry;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendition {
    /// Suffix after `+`, e.g. `480p`. Must be a valid stream-name fragment.
    pub label: String,
    pub height: u32,
    pub video_kbps: u32,
    pub audio_kbps: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ladder {
    /// Source stream names or `prefix*` patterns.
    pub streams: Vec<String>,
    pub renditions: Vec<Rendition>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Ffmpeg,
    RustyH264,
}

#[derive(Debug, Clone)]
pub struct TranscodeConfig {
    pub ladders: Vec<Ladder>,
    pub engine: Engine,
    /// ffmpeg binary for the `Ffmpeg` engine (and audio with `RustyH264`).
    pub ffmpeg: PathBuf,
    pub buffer: caudal_core::BufferConfig,
}

/// Starts transcoding matching streams (current and future). Must be called
/// inside a tokio runtime.
pub fn start(registry: Arc<Registry>, cfg: TranscodeConfig) -> std::io::Result<()> {
    let _ = (registry, cfg);
    Err(std::io::Error::other("transcoding not implemented yet"))
}
