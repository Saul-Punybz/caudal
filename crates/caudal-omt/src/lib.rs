//! Open Media Transport (OMT) for Caudal: pull OMT sources into streams and
//! send streams out as OMT sources.
//!
//! Modules so far:
//!
//! - [`ingest`]: `[[omt.pull]]` — OMT source → VMX/FPA1 decode → [`feed`]
//!   → published H.264/AAC stream, one thread per pull, with reload, tally
//!   and counters ([`PullStats`]).
//! - [`feed`]: raw video + float PCM with timestamps → one ffmpeg →
//!   H.264/AAC stream published in the registry (the ingest path's encoder).
//! - [`audio`]: OMT planar float audio → the feed's audio chunks.
//! - [`time`]: OMT 100 ns timestamps → Caudal track clocks (90 kHz video,
//!   sample-rate audio, shared origin, discontinuity re-anchoring).

pub mod audio;
pub mod feed;
pub mod ingest;
pub mod time;

pub use feed::{AudioFrame, Feed, FeedConfig, PixelLayout, PushError, SampleLayout, VideoFormat, VideoFrame};
pub use ingest::{DropReason, PullConfig, PullHandle, PullStats, PullStatsSnapshot, PullStatus, start_pulls};
pub use open_media_transport::command::Quality;
pub use time::TimeMap;
