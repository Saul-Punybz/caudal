//! Open Media Transport (OMT) for Caudal: pull OMT sources into streams and
//! send streams out as OMT sources.
//!
//! Modules so far:
//!
//! - [`feed`]: raw video + float PCM with timestamps → one ffmpeg →
//!   H.264/AAC stream published in the registry (the ingest path's encoder).
//! - [`output`]: registry streams out as OMT sources (H.264 → VMX,
//!   AAC/Opus → float PCM).
//! - [`time`]: OMT 100 ns timestamps → Caudal track clocks (90 kHz video,
//!   sample-rate audio, shared origin, discontinuity re-anchoring).

pub mod feed;
pub mod output;
pub mod time;

pub use feed::{AudioFrame, Feed, FeedConfig, PixelLayout, PushError, SampleLayout, VideoFormat, VideoFrame};
pub use output::{OutputConfig, OutputHandle, OutputOptions, OutputStats, OutputStatus, start_outputs};
pub use time::TimeMap;
