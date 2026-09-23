# caudal-transcode notes

## Process layout (Ffmpeg engine)

One ffmpeg per source, producing every rendition: one decode, `split` +
`scale=-2:<h>,format=yuv420p` per rung, one libx264 (`veryfast`,
`zerolatency`, `-bf 0`, `-g`/`-keyint_min` = 2 s of frames,
`-sc_threshold 0`, plus `-force_key_frames` every 2 s of media time so all
rungs cut on the same frames) and one AAC encode per rung.

- **Input:** stdin. MPEG-TS from `caudal_ts::mux::TsMux` when the source
  audio is AAC (or absent). TsMux drops Opus, so for Opus sources (WHIP) the
  input is a minimal live Matroska stream written by `src/mkv.rs` instead;
  that is how a WHIP source's audio becomes AAC (and Safari-audible).
- **Output:** ONE MPEG-TS on stdout carrying all rungs, one PID per
  elementary stream in output order (`-mpegts_start_pid 0x100`: v0 a0 v1 a1
  ...). The reader routes packets by PID (PAT and PMT to every rung) into one
  `TsDemux` + `Demuxer` per rung. No named pipes, no extra fds, one process
  and one decode per source; the CPU cost is one decode + N encodes. (One
  ffmpeg per rung would be simpler to read but pays N decodes.)
- **Lifetime:** spawned with `process_group(0)`; the whole group is
  SIGKILLed (via `kill -KILL -<pgid>`, no `unsafe`) when the source ends
  (after up to 3 s for ffmpeg to flush), when the task is dropped, and on
  crash. The child is reaped afterwards. Crash while the source is live →
  restart with backoff 0.5 s doubling to 10 s (reset after a 20 s healthy
  run); the rendition streams stay published across restarts. ffmpeg's
  stderr goes to `debug` logs; the last lines are logged at `warn` on a
  crash. The process carries `-metadata service_name=<source>` so operators
  (and the tests) can find it with `pgrep -f "service_name=<source> pipe:1"`.
- **Backpressure:** input goes to a bounded queue; if ffmpeg falls behind,
  the feeder drops frames up to the next source keyframe instead of growing
  memory or stalling the source.

The process handling, the TS reader, the rendition publisher, the clock
shift and the Matroska writer are public in `src/pipe.rs`
(`caudal_transcode::pipe`), shared with `caudal-omt`'s raw-frame feed.

## Timestamp alignment

Renditions carry the **source's own clock** (same frame → same time), so a
player can switch rungs cleanly:

1. Frames go into ffmpeg shifted onto a private clock: `t - origin + 10 s`,
   where `origin` is the first source frame ever fed (fixed for the life of
   the source, so restarts continue on the same timeline; the 10 s headroom
   keeps slightly older audio from going negative on the 33-bit TS clock).
2. ffmpeg runs with `-copyts -fps_mode passthrough` and the TS muxer with
   `-mpegts_copyts 1 -muxdelay 0 -muxpreload 0`, so output PTS equal input
   PTS exactly (checked: a 21.4 s input comes out at 21.4 s; AAC comes out
   one frame earlier because of encoder priming, ~21 ms).
3. The per-rung `Demuxer` rebases to its first unit's timestamp; we record
   that raw value and add it back, then undo step 1.

The test publishes a source with an arbitrary 123.456789 s offset and checks
every rendition video frame lands within 2 ms of a source frame time (Matroska
input is millisecond-precise, TS input is exact).

## RustyH264 engine

`rusty_h264 = "=0.16.0"`, `default-features = false, features = ["std"]`:
its defaults install a process-wide `#[global_allocator]` (not acceptable in
a library) and pull `rusty_h264-accel` (nasm-built asm; `nasm` is not
installed on this Mac). So this is the pure safe-Rust scalar build.

Pipeline, on one OS thread per source: decode each access unit
(`Decoder::decode`, AVCC → Annex B with SPS/PPS before keyframes) → reorder
by PTS (depth 4 once B-frames are seen) → bilinear scale (`src/scale.rs`, 2x2
box pre-shrink when shrinking over 2x) → one `Encoder` per rung (High/CABAC,
ABR at `video_kbps`, `lookahead = 0`, `scenecut = 0`, no B-frames, IDR forced
every 2 s of source time on all rungs together). Output PTS = the source
picture's PTS. Audio: AAC passes through untouched; other audio codecs are
dropped with an info log (use the Ffmpeg engine for Opus sources). H.265
sources are refused with an error log. Works live: the test transcodes the
256x144 B-frame fixture to 96p in a debug build without dropping a frame.

## Benchmark (18 Sep 2026, this Mac, load ≈ 3)

Same 10 s 1280x720 30 fps clip (`testsrc2`, x264 veryfast, no B-frames,
4 Mb/s) to 640x360 at 1000 kb/s, video only, keyframe every 60 frames. PSNR
with `ffmpeg -lavfi psnr` against the source scaled to 640x360 (bilinear).

| | wall | CPU (user+sys) | CPU% | output | PSNR Y / avg |
|---|---|---|---|---|---|
| ffmpeg libx264 veryfast zerolatency (default threads) | 0.23 s | 1.18 s | ≈ 510% | 1.02 Mb/s | 45.32 / 43.20 dB |
| ffmpeg libx264, 1 thread | 0.38 s | 0.75 s | ≈ 200% | 1.01 Mb/s | 45.40 / 43.26 dB |
| rusty_h264 0.16 scalar (decode + scale + encode, 1 thread) | 2.06 s | 2.04 s | ≈ 100% | 1.00 Mb/s | 40.65 / 39.37 dB |

rusty_h264 per frame: decode 2.00 ms, scale 0.29 ms, encode 4.56 ms
(≈ 146 fps, ≈ 4.9x real time for one 720p source → one 360p rung). Release
build with `CARGO_PROFILE_RELEASE_LTO=thin`.

Reading: rusty_h264 (scalar) costs ≈ 2–3x the CPU of ffmpeg/x264 and gives
≈ 4.7 dB less luma PSNR at the same bitrate (part of the gap is our bilinear
scaler vs ffmpeg's bicubic; both are measured against an ffmpeg-bilinear
reference). It is usable for small ladders where shipping without an ffmpeg
binary matters; ffmpeg stays the default. Its SIMD features were not
measured (they need the global allocator / nasm route above).
