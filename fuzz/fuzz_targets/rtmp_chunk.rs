//! Fuzzes the RTMP chunk stream reader: `vendor/scuffle-rtmp`'s
//! `ChunkReader`, patched (see `vendor/README.md`) for a Type 3 timestamp
//! bug. This is the very first thing untrusted bytes from an RTMP publisher
//! go through, before any FLV/AMF parsing (see `rtmp_flv_amf`), so it is
//! the target most likely to be reachable by an attacker who never
//! completes a real publish.
//!
//! Feeds the raw input as one buffer and drains every chunk `read_chunk`
//! will give up, the same loop `caudal-rtmp` would run as more bytes arrive
//! on the socket (see `crate::session` in `caudal-rtmp`, which calls this
//! through `scuffle_rtmp::ServerSession`). A crash here is a
//! pre-authentication remote DoS/memory-safety bug.

#![no_main]

use bytes::BytesMut;
use libfuzzer_sys::fuzz_target;
use scuffle_rtmp::chunk::reader::ChunkReader;

fuzz_target!(|data: &[u8]| {
    let mut buf = BytesMut::from(data);
    let mut reader = ChunkReader::default();

    // A real client negotiates a chunk size before sending large chunks;
    // derive one deterministically from the input so a single corpus entry
    // can still exercise more than the 128-byte default.
    if let Some(&first) = data.first() {
        const SIZES: [usize; 4] = [128, 1024, 4096, 65536];
        let _ = reader.update_max_chunk_size(SIZES[first as usize % SIZES.len()]);
    }

    // `read_chunk` never blocks: `Ok(None)` means "not enough data yet",
    // which for a fixed-size buffer means we are done.
    for _ in 0..10_000 {
        match reader.read_chunk(&mut buf) {
            Ok(Some(_chunk)) => continue,
            Ok(None) | Err(_) => break,
        }
    }
});
