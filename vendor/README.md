# Vendored crates

## scuffle-rtmp 0.2.3 (MIT OR Apache-2.0), patched

Upstream `ChunkReader` returned the previous header unchanged for every Type 3
chunk. That is right for a continuation chunk, but a Type 3 chunk that starts a
new message must add the last timestamp delta again (RTMP spec 5.3.1.2.4; FFmpeg
`libavformat/rtmppkt.c` does the same). Encoders that send constant-rate frames
as Type 3 (rml_rtmp, which `caudal-restream` uses) got frozen timestamps.

Patch: `src/chunk/reader.rs`, every change marked `Caudal patch`, plus the test
`test_reader_type3_new_messages_reuse_the_delta`. Found 18 Sep 2026 by
`crates/caudal-restream/tests/loopback.rs`. Drop this copy once upstream ships
a fix.
