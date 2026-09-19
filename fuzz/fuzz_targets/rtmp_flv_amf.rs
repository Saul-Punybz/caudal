//! Fuzzes `caudal-rtmp`'s FLV tag / AMF0 parsing (`crates/caudal-rtmp/src/demux.rs`):
//! `demux_video`, `demux_audio`, `parse_cue_point` and `parse_metadata_fps`.
//! These run on whatever bytes made it out of the chunk stream (see
//! `rtmp_chunk`), on every publish, before any codec-level decode.
//!
//! Reached through `caudal_rtmp::fuzz`, a `#[doc(hidden)]` module added
//! only for this: the real functions are `pub(crate)`.
//!
//! Input layout (deliberately simple so a real FLV tag body can be dropped
//! straight into a corpus file): `[op: u8][timestamp_ms: i64 LE][payload]`.
//! `op % 4` selects which function to call; `payload` is the FLV
//! `VIDEODATA`/`AUDIODATA` body or AMF0 message bytes.

#![no_main]

use bytes::Bytes;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&op, rest)) = data.split_first() else { return };
    let (ts_bytes, payload) = if rest.len() >= 8 { rest.split_at(8) } else { (&rest[..0], rest) };
    let timestamp_ms = ts_bytes.try_into().map(i64::from_le_bytes).unwrap_or(0);
    let payload = Bytes::copy_from_slice(payload);

    match op % 4 {
        0 => caudal_rtmp::fuzz::demux_video(timestamp_ms, payload),
        1 => caudal_rtmp::fuzz::demux_audio(payload),
        2 => caudal_rtmp::fuzz::parse_cue_point(timestamp_ms, payload, u32::from(op)),
        _ => caudal_rtmp::fuzz::parse_metadata_fps(payload),
    }
});
