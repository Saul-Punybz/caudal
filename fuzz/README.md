# Fuzzing Caudal's network parsers

Closes gap 3 of `docs/research/TEST-AUDIT.md` ("no fuzzing of network
parsers", which broke `PLAN.md`'s own rule: "Every parser gets a fuzz target
before it touches the network"). This is a separate Cargo workspace (see the
root `Cargo.toml`'s `exclude`), the standard `cargo-fuzz` layout, so it never
affects `cargo build`/`cargo test` on the main crates.

## Setup

`cargo-fuzz` needs the nightly toolchain (for `-Z sanitizer=address` and
libFuzzer's coverage instrumentation), independent of the pinned stable MSRV
(1.95) the rest of the repo builds with:

```sh
rustup toolchain install nightly
cargo install cargo-fuzz   # or: cargo binstall cargo-fuzz
```

## Targets

| Target | Crate(s) | Exercises |
|---|---|---|
| `rtmp_chunk` | `vendor/scuffle-rtmp` | The RTMP chunk stream reader (`chunk::reader::ChunkReader::read_chunk`) directly: chunk basic/message headers (Types 0-3), extended timestamps, multiplexed chunk stream IDs, partial-chunk reassembly. The very first thing bytes from an RTMP publisher go through, pre-authentication. |
| `rtmp_flv_amf` | `caudal-rtmp` | FLV tag / AMF0 parsing (`demux::demux_video`, `demux_audio`, `parse_cue_point`, `parse_metadata_fps`): legacy and Enhanced RTMP video/audio tags (H.264/H.265/AV1/AAC), `onCuePoint`/`onAdCue`/`onMetaData` AMF0 messages. Reached through `caudal_rtmp::fuzz`, a `#[doc(hidden)]` module added only for this (the real functions are `pub(crate)`). |
| `ts_demux` | `caudal-ts` | The MPEG-TS ingest pipeline end to end: `ts::TsDemux` (raw TS packets -> PES/PSI-section elementary-stream units, including SCTE-35 section reassembly on a PMT-declared PID) feeding straight into `demux::Demuxer` (Annex B -> AVCC/hvcC, ADTS -> raw AAC, `caudal-scte35` cue placement). What an SRT publisher's bytes go through. |
| `scte35_parse` | `caudal-scte35` | `parse` (a raw `splice_info_section`, CRC checked), `decode_text` (hex/base64 text form), `retime` (rewrites `pts_adjustment`). Reachable from an MPEG-TS SCTE-35 PID, an RTMP `onCuePoint`, or any HTTP API that accepts a cue by text. |
| `omt_time_map` | `caudal-omt` | `time::TimeMap`: OMT 100 ns timestamps from any sender (backwards, repeated, huge jumps, bad sample rates) onto 90 kHz video / sample-rate audio. Asserts video pts strictly increase and audio chunks never overlap, which the ffmpeg feed relies on. |
| `rtsp_request` | `caudal-rtsp` | RTSP server-side request parsing: `rtsp-types`'s wire parser plus `caudal-rtsp`'s own URI/query parsing (`server::parse_uri`) and `Transport` header selection (`server::choose_transport`). Reached through `caudal_rtsp::fuzz` (`server` is a private module). |
| `webrtc_sdp` | `caudal-webrtc` | WHIP/WHEP SDP offer handling: `str0m`'s offer parse + ICE-lite answer (via `caudal-webrtc::negotiate`), plus the answer-side `answer_codecs` line scan. Never touches `engine.rs` (the UDP peer loop, owned by other work) — no socket, no peer, just offer parsing and answer generation. Reached through `caudal_webrtc::fuzz`. |

Not yet covered (audit's remaining targets, left for a follow-up pass):
Opus TOC (`caudal-cmaf/src/opus.rs`), avcC/hvcC record parsing in isolation
(exercised indirectly today through `rtmp_flv_amf`'s video-init path and
`ts_demux`'s SPS/PPS handling, but not as their own targets), MP4 demux in
`caudal-hls`/`mp4demux` (that crate is other work's, off limits this pass),
ADTS header parsing in isolation (exercised indirectly through `ts_demux`'s
audio path), SRT streamid parsing, token/`Authorizer::check`.

Every `caudal_rtmp::fuzz`, `caudal_rtsp::fuzz` and `caudal_webrtc::fuzz`
module is `#[doc(hidden)] pub mod fuzz` added specifically so this crate can
reach otherwise-private parsing internals without loosening the crate's real
public API. They must never panic on any input themselves (they only unwrap
things this crate's own type system already guarantees, like a freshly
constructed `Registry`).

## Running

One target at a time (see the machine rules this was developed under: single
worker, capped RSS, always a time limit):

```sh
cd fuzz
cargo +nightly fuzz run <target> -- -max_total_time=300 -jobs=1 -workers=1 -rss_limit_mb=2048
```

On Apple Silicon, `cargo-fuzz` picks the wrong default target triple; add
`--target aarch64-apple-darwin` (or whatever `rustc -vV`'s `host:` line
says) to both `build` and `run`.

Minimize and turn a crash into a regression test:

```sh
cargo +nightly fuzz tmin <target> artifacts/<target>/crash-... [-- --target aarch64-apple-darwin]
```

Then write a unit test in the *owning* crate (not here) from the minimized
bytes, confirm it fails before the fix and passes after, fix the parser
(never `catch_unwind`), and rerun the target to confirm the crash is gone.

## Seed corpora

`corpus/<target>/` holds real or realistically-shaped inputs, so coverage-
guided mutation starts from something structurally valid instead of noise:

- `rtmp_chunk/`: a hand-built RTMP chunk stream (video/audio/AMF0 messages
  across the video/audio/data chunk stream IDs, one message big enough to
  need Type 3 continuation chunks), built from real FLV tag payloads (see
  next entry) via `chunk::writer`-equivalent framing.
- `rtmp_flv_amf/`: real FLV tag bodies — an `AVCDecoderConfigurationRecord`
  and a keyframe NALU built from `crates/caudal-hls/tests/fixtures/av.mp4`'s
  actual H.264 SPS/PPS/IDR (via `ffmpeg ... -bsf:v h264_mp4toannexb`), a real
  AAC `AudioSpecificConfig` and raw access unit derived from that fixture's
  audio track's ADTS header, and hand-built (but spec-shaped) AMF0
  `onCuePoint`/`onMetaData` messages. Each file is `[op][timestamp_ms LE
  i64][FLV tag body]` — see the target's own doc comment for the `op`
  encoding.
- `ts_demux/`: two real MPEG-TS captures (`ffmpeg -c copy -f mpegts` from the
  same fixture, and a `testsrc2`+`sine` synthetic one) plus one hand-built TS
  with a real PAT/PMT (CRC included) declaring an SCTE-35 PID (stream_type
  `0x86`) carrying the crate's own SCTE-35 test vector — needed because nothing
  in `ts::TsDemux` looks at that PID until a PMT says to.
- `scte35_parse/`: the crate's own `splice_insert` test vector, as a raw
  section, as hex text, as base64 text, and as a `retime` call.
- `rtsp_request/`: real RTSP/1.0 request text for every method the server
  handles (OPTIONS, DESCRIBE with `?token=`, SETUP over UDP and TCP
  interleaved, PLAY with a Range header, TEARDOWN, GET_PARAMETER).
- `omt_time_map/`: `[op][ts][a][b]` records shaped like a 30 fps source
  with 48 kHz audio chunks, a sender restart (clock back to 0) and a 1 h
  jump; and 59.94 fps with 44.1 kHz odd-sized chunks and a rate change.
- `webrtc_sdp/`: a realistic WHIP-shaped SDP offer (H.264 packetization-mode
  1 + Opus, ICE ufrag/pwd, DTLS fingerprint) and a small answer-shaped text
  sample for the `answer_codecs` path.

## Findings so far

Seven crashes, all pre-authentication (any RTMP publisher, no token check
runs before the chunk stream or an FLV tag is parsed), found across
`rtmp_chunk` and `rtmp_flv_amf` runs against the seed corpus above — the
first two directly in `rtmp_chunk`, the rest one call chain deeper each
time: fixing one let `rtmp_flv_amf` run further into
`caudal-rtmp::demux::video_dimensions`'s H.264 SPS parsing and find the
next. Every fix and its regression test are documented in full in
`vendor/README.md` (for the four vendored crates) or inline in
`crates/caudal-rtmp/src/demux.rs` (for the one bug in Caudal's own code);
short version, in the order found:

1. `vendor/scuffle-rtmp/src/chunk/reader.rs`: a Type 0/1/2 header restating
   a *smaller* `msg_length` than a same-key message already partway
   buffered underflowed a `usize` subtraction in `get_payload_range`.
2. `vendor/scuffle-rtmp/src/chunk/reader.rs`: a Type 2 chunk's timestamp
   delta was added to the previous timestamp with a plain `+`, panicking
   once the 32-bit millisecond counter (which legitimately wraps every 49.7
   days) overflowed.
3. `vendor/scuffle-amf0/src/decoder.rs`: an AMF0 `EcmaArray`/`StrictArray`'s
   4-byte element count was used unclamped as a `with_capacity` hint —
   worse than the others, a failed allocation **aborts the whole process**
   (not a catchable panic), not just one connection.
4. `crates/caudal-rtmp/src/demux.rs` (Caudal's own code, not vendored): the
   RTMP-ms-to-90kHz-tick conversion (`timestamp_ms * 90`/`* 1000`) had no
   overflow guard against `RtmpClock`'s ever-growing extended timestamp.
5. `vendor/scuffle-h264/src/sps/mod.rs`: `Sps::width()`/`height()` computed
   `base - crop_offset * 2` with plain arithmetic; crop offsets bigger than
   the frame (legal to parse, nonsensical semantically) underflowed it.
6. `vendor/scuffle-expgolomb/src/lib.rs`: `read_exp_golomb` built its result
   as `1 << leading_zeros` with no cap; 64+ leading zero bits wrapped it to
   0 and the following `- 1` underflowed.
7. `vendor/scuffle-h264/src/sps/sps_ext.rs`: the scaling-matrix loop's
   `next_scale = next_scale + delta_scale + 256` overflowed for a
   `delta_scale` near `i64::MIN`/`i64::MAX` (itself a legal
   `read_signed_exp_golomb` result after fixing #6).

Execs and results for every target's final, clean 5-minute run
(`-max_total_time=300 -jobs=1 -workers=1 -rss_limit_mb=2048`, one at a
time, against the seed corpus above), 19 Sep 2026:

| Target | Execs | Notes |
|---|---|---|
| `rtmp_chunk` | 12,022,263 | Clean only after fixes #1–#2. |
| `rtmp_flv_amf` | 18,706,641 | Clean only after fixes #3–#7 (found in that order, one rebuild-and-rerun per fix). |
| `ts_demux` | 114,497 | Clean first run. Fewer execs: the seed corpus's real TS captures are much larger (44–121 KB) than the other targets', so each mutation/exec costs more. |
| `scte35_parse` | 149,543,561 | Clean first run. |
| `rtsp_request` | 10,755,137 | Clean first run. |
| `webrtc_sdp` | 853,939 | Clean first run. Fewer execs: `str0m`'s SDP offer parse + ICE-lite `accept_offer` is the heaviest single call in any target here. |

All seven fixes are also covered by unit tests in their owning crate
(`cargo test`/`cargo nextest run` from the repo root); `cargo fmt --all
--check`, `cargo clippy --workspace --all-targets -- -D warnings` and
`cargo nextest run --workspace` (plus `-p caudal` with `CAUDAL_E2E=1`) were
all green after every fix.
