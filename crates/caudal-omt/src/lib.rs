//! Open Media Transport (OMT) for Caudal: pull OMT sources into streams and
//! send streams out as OMT sources.
//!
//! Modules so far:
//!
//! - [`feed`]: raw video + float PCM with timestamps → one ffmpeg →
//!   H.264/AAC stream published in the registry (the ingest path's encoder).
//! - [`time`]: OMT 100 ns timestamps → Caudal track clocks (90 kHz video,
//!   sample-rate audio, shared origin, discontinuity re-anchoring).

pub mod feed;
pub mod time;

pub use feed::{AudioFrame, Feed, FeedConfig, PixelLayout, PushError, SampleLayout, VideoFormat, VideoFrame};
pub use time::TimeMap;
