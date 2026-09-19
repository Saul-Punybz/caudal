# Caudal vs MediaMTX v1.21: side-by-side benchmark

Measured 18 Sep 2026 on one machine, one server at a time, release builds,
same input. Reproduce with one command: `bench/run.sh` (about 80 minutes;
options pass through to `bench/bench.py run`, e.g.
`bench/run.sh --reps 1 --levels 1,100 --protos hls`). Raw data:
`bench/results/20260918-202735.jsonl` (main run) and
`bench/results/20260918-214620.jsonl` (buffer sensitivity run), with the
generated tables next to them (`.md`). The latency rows were re-measured
the same evening with a corrected decoder command
(`bench/results/20260918-225809.jsonl`); see Latency.

## Short answer

| Metric | Caudal | MediaMTX | Verdict |
|---|---|---|---|
| Binary size | 21.8 MB | 54.1 MB | **Caudal 2.5x smaller** |
| Idle RSS | 14.0 MB | 35.4 MB | **Caudal 2.5x less** |
| RSS with 1 publisher (1080p30, 6 Mbps) | 97.9 MB (86.8 MB with a 14 s buffer) | 80.3 MB | **MediaMTX lighter** |
| RSS with 1 publisher, steady state (Linux runner, after the 19 Sep memory fixes) | 65.4 MB (145 before) | 92.7 MB | **Caudal about 30 % less** (see "Memory per live stream after the fixes") |
| CPU with 1 publisher | 1.0 % | 1.5 % | tie (both negligible) |
| LL-HLS, 1,000 viewers: server CPU | 83 % | 168 % | **Caudal about 2x less CPU** |
| LL-HLS, 1,000 viewers: server RSS | 215 MB | 339 MB | **Caudal less** |
| LL-HLS, 100 / 300 viewers: server CPU | 8.7 % / 26.5 % | 26.7 % / 72.4 % | **Caudal about 3x less** |
| RTSP (TCP), 100 / 300 viewers: server CPU (M4, before the send-path fix) | 122 % / 319 % | 104 % / 278 % | MediaMTX about 15 % less |
| RTSP (TCP), 100 / 300 viewers: server CPU (Linux runner, after the fix, 19 Sep) | 11.5 % / 29.9 % | 63.0 % / 184 % | **Caudal about 5–6x less** (see "RTSP after the send-path fix") |
| RTSP, 1,000 viewers | 14 of 1,000 kept up | 0 of 1,000 kept up | neither works here; the machine's network memory is the limit (see below) |
| WHEP, 100 viewers: CPU / RSS (after the WebRTC fixes, 19 Sep) | 70 % / 364 MB | 69 % / 513 MB | tie on CPU, **Caudal less memory** |
| WHEP, 300 viewers (after the fixes) | **1,799 Mbps, 300 of 300 kept up**, 315 % CPU, 829 MB | 1,666 Mbps, 262 of 300 kept up (0 in one run), 178 % CPU, 1,394 MB | **Caudal delivers every viewer, with less memory and more CPU** |
| WHEP, 1,000 viewers | not re-measured (the laptop, not the server, is the limit) | 1,269 Mbps, 4.4 GB RSS | see "WHEP after the fixes" |
| LL-HLS latency, live edge (median / p95) | 212 / 224 ms | 210 / 223 ms | tie (the earlier 145 ms gap was the decoder, not the server; see Latency) |
| RTSP latency (median / p95) | 45 / 55 ms | 45 / 56 ms | tie |

"Faster" is supported for **LL-HLS fan-out CPU and memory**, **WHEP delivery
at 300 viewers** (every viewer kept up, less memory, but more CPU), binary
size and idle memory. On RTSP fan-out CPU and memory with one live stream,
MediaMTX was better on 18 Sep; after the 19 Sep send-path fix, measured on
GitHub's Linux runners, Caudal's RTSP fan-out uses about a fifth of
MediaMTX's CPU (see "RTSP after the send-path fix"). Latency is a tie.

## Setup

**Machine** (`sysctl`): Apple M4, 10 cores (4 performance + 6 efficiency),
24 GB, macOS 26.5.1 (25F80). `kern.ipc.nmbclusters` 131072 (about 250 MB of
kernel network memory), `kern.ipc.maxsockbuf` 8 MB, ephemeral ports
49152–65535. The harness raises `ulimit -n` to 65536; the file-descriptor
limit was never hit.

**Not idle.** The laptop was shared: other processes (Chrome, WindowServer,
other sessions) used a median of 1.3 to 3.3 cores during the measurement
windows (`other_cpu_pct_median` in the JSONL; `ps` decaying average). That
leaves about 7 cores for server, load client and publisher, and adds noise
that the three repetitions show as spread. Results are comparable between
the two servers (same conditions, interleaved order) but not as absolute
capacity numbers.

**Versions.** Caudal `e1dc82f` built with `cargo build --release -p caudal`
(rustc 1.98.1; LTO fat, 1 codegen unit, stripped). MediaMTX v1.21.0
`darwin_arm64`, the official release, SHA-256 checked against the release's
`checksums.sha256` by `bench/fetch-mediamtx.sh`. FFmpeg 9.0.1 (Homebrew).

**Configs.** `bench/caudal.toml` and `bench/mediamtx.yml`. Everything is on
127.0.0.1. LL-HLS on both, 200 ms parts, 2 s segments (keyframe every 2 s,
so segments are 2 s on both). MediaMTX `hlsAlwaysRemux: true` so it packages
from the moment a stream is published, as Caudal always does. Both run
their default set of listeners (RTMP, RTSP, LL-HLS, WebRTC, SRT, MoQ).
Differences that remain:
- Caudal keeps a 50 s live buffer (its shipped default); MediaMTX keeps 7
  segments (about 14 s). A sensitivity run with `bench/caudal-14s.toml`
  (buffer 14 s) is reported below.
- MediaMTX serves audio as a separate LL-HLS rendition (its own playlist
  and parts); Caudal muxes audio and video in one. A player (and the load
  client) follows two playlists on MediaMTX and one on Caudal. This is why
  MediaMTX's per-viewer LL-HLS egress is 6.47 Mbps against Caudal's 6.23.
- MediaMTX's PART-HOLD-BACK is 0.5 s; Caudal's is 0.601 s.
- WHEP carries video only on both: the source's AAC is not transcoded to
  Opus by either server.

**Input.** A 120 s file rendered once by `bench/bench.py prepare`: testsrc2
1920x1080 at 30 fps with temporal noise (so the encoder needs its bits),
libx264 veryfast, zerolatency, High profile, 6 Mbps (maxrate 6M, bufsize
3M), GOP 60 (2 s), no B-frames, plus a 1 kHz sine as AAC-LC 128 kb/s 48 kHz.
Measured: video 6.01 Mbps, audio 0.13 Mbps. Throughput runs publish it with
`ffmpeg -re -stream_loop -1 -i src.mp4 -c copy -f flv rtmp://.../live/bench`
(about 1 % CPU, so the encoder does not compete with the server). Latency
runs encode live with the same settings.

## Method

- **Every scenario starts a fresh server and a fresh publisher**, then
  waits until the LL-HLS media playlist has parts. One server runs at a
  time. Order per repetition: idle+publish (Caudal, MediaMTX), then fan-out
  per protocol and level, alternating servers, then latency. Three
  repetitions of everything; cells show median (min–max) across them.
- **CPU** = delta of cumulative CPU time (`ps -o time`) over the window,
  in % of one core (1,000 % = 10 cores). **RSS** sampled every second.
  The same is recorded for the load client and the publisher.
- **Idle**: 15 s window, 5 s after start. **One publisher**: 30 s window
  after the stream is live plus 10 s.
- **Fan-out**: viewers join over a ramp (10 ms apart, at most 10 s), then
  10 s of warm-up, then a 30 s window (28 s of process sampling inside it).
  Levels 1, 100, 300 and 1,000 (300 was added because 1,000 hit machine
  limits). Egress = payload bytes the client received per second (HTTP
  bodies for LL-HLS, RTP payloads for RTSP/WHEP). **Kept up** = viewers
  that received at least 90 % of real time: for LL-HLS, media duration
  fetched divided by wall time; for RTSP/WHEP, at least 90 % of the
  source's bitrate.
- **Load client** (`bench/client`, Rust, standalone crate with its own
  lockfile, one process, one tokio task per viewer):
  - LL-HLS: behaves like a low-latency player: reads the multivariant
    playlist, follows every media playlist of the first variant with
    blocking reloads (`_HLS_msn`/`_HLS_part`) and fetches every new part,
    starting at the live edge; one HTTP connection pool per viewer. It does
    not fetch preload hints.
  - RTSP: a minimal client written for this (DESCRIBE, SETUP per track with
    TCP interleaving, PLAY), which counts RTP payload bytes and sequence
    gaps. retina was used first but aborts a session on the first
    sequence gap, which under overload turned "the server dropped packets"
    into "the session failed"; a load client must count loss, not stop.
  - WHEP: str0m 0.23.1 in RTP mode, one UDP socket per viewer, recvonly
    video and audio. It offers H.264, Opus, PCMU and PCMA: MediaMTX
    rejects an offer whose audio section can't carry its placeholder
    PCMU track ("codecs not supported by client").
  - Why not an existing tool: nothing installable here speaks LL-HLS
    blocking reloads (wrk, hey, oha, k6 were not installed and are plain
    HTTP load tools anyway); running 1,000 ffmpeg readers would need about
    25 GB of RAM on a 24 GB machine.
- **Kernel counters** per window: `netstat -m` "requests for memory
  denied" (mbuf) and `netstat -s -p udp` "dropped due to full socket
  buffers", as deltas. They show when the machine, not the server, is the
  limit.
- **Latency**: the source burns a 32-bit stamp into the top 40 rows of
  each frame: bit k of (wall-clock ms mod 2^32) as a 60 px black or white
  block, taken with ffmpeg's `setpts=RTCTIME` and drawn with `geq` before
  encoding. The viewer decodes the stamp from ffmpeg's gray output and
  subtracts it from its own clock (same machine, same clock, no OCR). One
  sample per second after a 3 s settle, 20 samples per run, 3 runs, so 60
  samples per cell. Paths:
  - `flv` (baseline, no server): encoder -> FLV over TCP -> decoder. This
    is the floor: encode, mux, decode, pipes.
  - `hls`: the client follows the video playlist from the live edge (last
    listed part, blocking reloads, no hold-back) and pipes init + parts
    into `ffmpeg -f mp4 -i pipe:0`. This is **edge latency**, lower than
    what a player shows: a player like hls.js also waits PART-HOLD-BACK.
  - `rtsp`: `ffmpeg -rtsp_transport tcp -fflags nobuffer -flags low_delay`.
  - Every path writes the stamp strip with `-fps_mode passthrough -threads 1`
    (added after the first run; see Latency for why).

## Results

### Binary, idle, one publisher

| Metric | Caudal | MediaMTX |
|---|---|---|
| Binary, MB | 21.84 | 54.09 |
| Idle RSS, MB | 14.0 (13.8–14.1) | 35.4 (34.9–35.4) |
| Idle CPU, % | 0.00 | 0.00 (0.00–0.10) |
| 1 publisher: RSS median, MB | 97.9 (97.7–99.2) | 80.3 (78.2–81.7) |
| 1 publisher: RSS max, MB | 126 (125–127) | 94.5 (93.2–97.0) |
| 1 publisher: CPU, % of one core | 1.0 (0.9–1.0) | 1.5 (1.4–1.6) |
| (publisher ffmpeg CPU, %) | 1.3 (1.2–1.3) | 1.1 (1.1–1.1) |

Sensitivity (Caudal with a 14 s buffer, `bench/caudal-14s.toml`, 3 runs):
1 publisher RSS median 86.8 MB (85.5–89.5), max 98.5 MB; LL-HLS x1,000 RSS
184 MB (183–187), CPU 86 % (78–95). The buffer explains about 11 MB of the
18 MB gap with one publisher; Caudal is still about 6 MB heavier than
MediaMTX per live stream at equal retention.

Note: `PLAN.md` expected 10–20 MB for the full binary; it is 21.8 MB now.

### Memory per live stream after the fixes (19 Sep 2026, Linux)

Measured on GitHub's `ubuntu-latest` runners with `.github/workflows/bench.yml`
(`only=idle servers=caudal,mediamtx reps=3`), not on the laptop. Compare
Caudal and MediaMTX inside each run.

**The M4 number was not steady state.** The publish window started 10 s
after the stream went live and lasted 30 s, so Caudal's 50 s buffer was only
12 to 42 s full. The new workflow input `settle` (`BENCH_SETTLE_S`) sets that
wait; `settle=60` measures a full buffer.

**Where the memory went.** A dhat heap profile of one publisher for 70 s
(`bench/heap.sh`, workflow input `heap=70`, summary by `bench/dhat_top.py`):
the heap peaked at 125.6 MB.

| Site | MB at peak | Why |
|---|---|---|
| RTMP ingest, frames in the live buffer | 64.0 | 38.6 MB of payload (50 s) in 64 MB of allocations: scuffle-rtmp hands out each message as a slice of a buffer grown by doubling, or of the socket read buffer, and the slice keeps all of it alive |
| MoQ publish | 37.4 + 3.1 | hang's default track cache is 30 s: a second copy of every stream, with or without MoQ viewers |
| LL-HLS parts | 10.7 | parts of the listed segments |
| LL-HLS whole segments | 9.2 | the same bytes again, concatenated |

**Fixes.** RTMP frame payloads are copied into exact-size allocations
(`crates/caudal-rtmp/src/demux.rs`, `own`). MoQ tracks cache 5 s, moq-net's
own default (`crates/caudal-moq/src/publish.rs`, `LATENCY_MAX`). A completed
LL-HLS segment's parts become slices of the whole segment
(`crates/caudal-hls/src/packager.rs`, `close_segment`). The default live
buffer is 15 s instead of 50 (`[buffer] window_secs`; reasons in
`caudal.example.toml`). After: heap peak 35.0 MB (frames 12.5, LL-HLS 12.3,
MoQ 9.3).

**Result**, median (min–max) of 3 repetitions:

| Build | Publish window | Caudal RSS MB | MediaMTX RSS MB (same run) | Caudal max MB | MediaMTX max MB |
|---|---|---|---|---|---|
| before (`main`) | 10 s after live (as on the M4) | 90.3 (90.2–90.6) | 84.5 (83.4–85.3) | 117 | 97.5 |
| after | 10 s after live | 54.7 (54.6–57.0) | 83.7 (83.4–83.7) | 64.2 | 99.9 |
| before (50 s buffer) | 60 s after live (steady) | 145 (145–151) | 93.6 (88.5–96.6) | 155 | 95.1 |
| after | 60 s after live (steady) | 65.4 (65.0–65.8) | 92.7 (90.4–96.4) | 67.8 | 96.8 |

Idle RSS in the same runs: Caudal 17.8–18.1 MB, MediaMTX 39.2–41.3 MB.
Publish CPU unchanged (Caudal 0.7–1.2 %, MediaMTX 0.9–1.9 %).

Runs: heap before
[35463795166](https://github.com/Saul-Punybz/caudal/actions/runs/35463795166),
heap after [35464901974](https://github.com/Saul-Punybz/caudal/actions/runs/35464901974);
RSS before [35463651012](https://github.com/Saul-Punybz/caudal/actions/runs/35463651012)
(settle 10, `main`) and
[35463800075](https://github.com/Saul-Punybz/caudal/actions/runs/35463800075)
(settle 60, bench tooling only, 50 s buffer); after
[35464905855](https://github.com/Saul-Punybz/caudal/actions/runs/35464905855)
(settle 10) and
[35464904097](https://github.com/Saul-Punybz/caudal/actions/runs/35464904097)
(settle 60).

Not measured: the fixes with the old 50 s buffer (estimated from the heap
profile: about 26 MB more, near MediaMTX's figure), the M4, fan-out RSS
after the change, and ingest other than RTMP (SRT, RTSP, WHIP may slice
their buffers the same way; not profiled).

### Fan-out

CPU in % of one core. Client CPU is the load client (it shares the machine).

| Proto | Viewers | Server | Server CPU % | Server RSS MB | Egress Mbps | Kept up | Errors | Timeouts | RTP lost | Client CPU % | mbuf denied | UDP drops |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| LL-HLS | 1 | Caudal | 1.1 (1.0–1.2) | 109 | 6.2 | 1 | 0 | 0 | — | 0.2 | 0 | 0 |
| LL-HLS | 1 | MediaMTX | 2.0 (1.8–2.2) | 91 | 6.5 | 1 | 0 | 0 | — | 0.4 | 0 | 0 |
| LL-HLS | 100 | Caudal | 8.7 (8.1–9.2) | 119 | 624 | 100 | 0 | 0 | — | 11.7 | 0 | 0 |
| LL-HLS | 100 | MediaMTX | 26.7 (26.1–29.9) | 129 | 647 | 100 | 0 | 0 | — | 19.5 | 0 | 0 |
| LL-HLS | 300 | Caudal | 26.5 (23.2–27.9) | 142 | 1,871 | 300 | 0 | 0 | — | 35.1 | 0 | 0 |
| LL-HLS | 300 | MediaMTX | 72.4 (69.2–74.8) | 178 | 1,945 | 300 | 0 | 0 | — | 49.9 | 0 | 0 |
| LL-HLS | 1,000 | Caudal | 83.0 (82.3–86.2) | 215 (213–216) | 6,233 | 1,000 | 0 | 0 | — | 88.4 | 19,888 | 0 |
| LL-HLS | 1,000 | MediaMTX | 168 (168–170) | 339 (333–350) | 6,471 | 1,000 | 0 | 0 | — | 122 | 0 | 0 |
| RTSP | 1 | Caudal | 2.2 (2.0–2.9) | 109 | 6.1 | 1 | 0 | 0 | 0 | 0.7 | 0 | 0 |
| RTSP | 1 | MediaMTX | 2.8 (2.7–2.9) | 92 | 6.1 | 1 | 0 | 0 | 0 | 0.6 | 0 | 0 |
| RTSP | 100 | Caudal | 122 (119–123) | 116 | 613 | 100 | 0 | 0 | 0 | 32.8 | 0 | 0 |
| RTSP | 100 | MediaMTX | 104 (96–106) | 109 | 613 | 100 | 0 | 0 | 0 | 28.4 | 0 | 0 |
| RTSP | 300 | Caudal | 319 (303–341) | 126 | 1,840 | 300 | 0 | 0 | 0 | 79.0 | 0 | 0 |
| RTSP | 300 | MediaMTX | 278 (278–279) | 134 | 1,842 | 300 | 0 | 0 | 0 | 69.2 | 0 | 0 |
| RTSP | 1,000 | Caudal | 304 (273–338) | 205 | 1,941 (1,785–2,028) | 14 (11–31) | 8 | 1,740 | 0 | 70.5 | 7,614 | 0 |
| RTSP | 1,000 | MediaMTX | 284 (210–293) | 213 | 2,509 (1,868–2,563) | 0 | 13,592 | 1 | 1.79 M | 53.5 | 46,072 | 0 |
| WHEP | 1 | Caudal | 4.6 (4.4–4.7) | 114 | 6.0 | 1 | 0 | 0 | — | 3.0 | 0 | 0 |
| WHEP | 1 | MediaMTX | 3.7 (3.2–3.9) | 96 | 6.0 | 1 | 0 | 0 | — | 2.4 | 0 | 0 |
| WHEP | 100 | Caudal | 87.4 (87.2–87.5) | 334 | 600 | 100 | 0 | 0 | — | 78.3 | 0 | 0 |
| WHEP | 100 | MediaMTX | 73.1 (72.8–73.1) | 524 | 600 | 100 | 0 | 0 | — | 69.9 | 0 | 0 |
| WHEP | 300 | Caudal | 46.9 (46.3–46.9) | 678 | 245 | 0 | 0 | 0 | — | 40.3 | 0 | 12,691 |
| WHEP | 300 | MediaMTX | 168 (168–169) | 1,404 | 1,377 | 0 | 0 | 0 | — | 158 | 0 | 0 |
| WHEP | 1,000 | Caudal | 130 (128–131) | 1,096 | 307 (228–373) | 0 | 323 | 1,116 | — | 58.4 | 0 | 97,745 |
| WHEP | 1,000 | MediaMTX | 177 (176–177) | 4,393 | 1,269 | 0 | 0 | 0 | — | 153 | 0 | 0 |

Full table with every spread: `bench/results/20260918-202735.md`.

What the table says:

- **LL-HLS is Caudal's clear win.** Per delivered gigabit, Caudal used
  13–14 % of a core at every level from 100 to 1,000 viewers; MediaMTX
  used 41 % at 100, 37 % at 300 and 26 % at 1,000, with less memory at 300 and 1,000 viewers and no errors on
  either. At 1,000 viewers (6.2 Gbps over loopback) the kernel refused mbuf
  allocations during Caudal's runs (about 20,000) but every viewer still
  kept up; MediaMTX's runs showed none.
- **RTSP costs Caudal about 15–17 % more CPU** than MediaMTX at 100 and 300
  viewers, with the same egress and no loss on either. Memory is about the
  same. (Fixed 19 Sep: see "RTSP after the send-path fix".)
- **RTSP at 1,000 viewers doesn't work on this machine, for either server.**
  Each run exhausted the kernel's network memory (mbuf denials in every
  run, 7,600 for Caudal and 46,000 for MediaMTX); `kern.ipc.nmbclusters` is
  a boot-time setting this session could not raise. The two fail
  differently: MediaMTX keeps sessions and drops packets for slow readers
  (1.8 M RTP packets lost, "reader is too slow" in its log, many
  reconnects), and in 1 of 3 runs the publisher itself died with ENOBUFS.
  Caudal stops feeding most sessions (1,740 read timeouts, 14 viewers kept
  up). Neither result measures the server; they measure this laptop.
- **WHEP at 100 viewers:** MediaMTX uses about 16 % less CPU, Caudal about
  36 % less memory.
- **WHEP at 300 and 1,000 viewers is a real Caudal problem.** At 300
  viewers Caudal delivered 245 Mbps (0.8 Mbps per viewer against a 6 Mbps
  stream) while using only 47 % of one core, with no thread saturated
  (per-thread `ps -M` in a separate probe: busiest thread 18 %), and the
  kernel counted 12,700 UDP datagrams dropped on full socket buffers.
  MediaMTX delivered 1,377 Mbps (4.6 Mbps per viewer) with no kernel drops.
  So Caudal is not CPU-bound here: it is stalling or dropping somewhere in
  its single-socket send/receive path. At 1,000 viewers Caudal's sessions
  time out (ICE consent / no media), while MediaMTX keeps every session
  with 1.3 Mbps each and 4.4 GB of RSS. Neither kept up at 300 or more,
  and the load client (str0m, 150 % CPU for MediaMTX's run) may be part of
  MediaMTX's limit; it is not what limits Caudal, which receives less than
  a fifth of the bytes.

### WHEP after the fixes (19 Sep 2026)

The collapse above had three causes, found by profiling (`sample` on a
release build with symbols) rather than guessed:

1. **Software AES on ARM.** The WebRTC thread spent its time in
   `aes::soft::fixslice` and `polyval::soft`: on aarch64, `aes` 0.8 and
   `polyval` 0.6 only use the ARMv8 AES/PMULL instructions when built with
   `--cfg aes_armv8 --cfg polyval_armv8` (now in `.cargo/config.toml`; still
   detected at run time). x86_64 detects AES-NI on its own, so this was
   ARM-only. The load client inherits the flags too.
2. **A blocking send path.** The engine awaited every `send_to`, macOS's
   default UDP send buffer is 9 KB, and a `biased` select served media before
   reading the socket. Now: non-blocking sends with an outbox, 8 MB socket
   buffers, burst reads. Kernel UDP drops went from 12,700 to 0.
3. **One core.** All peers ran on one engine. Engines now run one per core
   (at most 8), each with its own socket on the same port (`SO_REUSEPORT`);
   the kernel may hand a datagram to any of them and it is forwarded to the
   owner by STUN ufrag or source address. A first version shared one socket
   across engines and got worse (8 engines, 320 % CPU at 100 viewers: the
   threads fought over the socket's send lock, `__sendto` dominated the
   profile). Sessions fill an engine to 50 before the next is used.

Results, 3 reps, same session (`bench/results/20260919-002645.jsonl`); I was
compiling other work during part of this run, but the ranges are narrow:

| Viewers | Server | CPU % (range) | RSS MB | Egress Mbps | Kept up |
|---|---|---|---|---|---|
| 100 | Caudal | 70.4 (70.3–72.0) | 364 | 600 | 100 |
| 100 | MediaMTX | 68.8 (68.5–77.0) | 513 | 600 | 100 |
| 300 | Caudal | 315 (313–342) | 829 | 1,799 | 300 |
| 300 | MediaMTX | 178 (172–179) | 1,394 | 1,666 (1,452–1,689) | 262 (0–288) |

At 1,000 viewers (one exploratory run) Caudal delivered 2,920 Mbps against
MediaMTX's 1,528, but the load client used 260 % CPU and the kernel dropped
338,805 datagrams: that cell measures the laptop. It needs a second machine.
Caudal still uses more CPU than MediaMTX at 300 viewers; the next profile
should look at per-packet allocation (`Vec` per datagram) and batched sends.

### RTSP after the send-path fix (19 Sep 2026, Linux)

Measured on GitHub's `ubuntu-latest` runners with `.github/workflows/bench.yml`
(`protos=rtsp levels=100,300 reps=3 only=fanout`), not on the laptop.
Shared runners are noisy between runs, so compare Caudal against MediaMTX
inside each run, not across runs.

**Where the CPU went.** `perf record -g` on the server during the RTSP x300
cell (workflow input `profile=rtsp:300`; release build with line tables):
86.7 % of Caudal's samples sat under `TcpStream::poll_write`, i.e. the send
syscall and the kernel TCP/loopback path below it. The play task wrote
every RTP packet with its own `write_all` (TCP_NODELAY is on, so one
`send` and one TCP segment per packet, about 600 per second per viewer at
6 Mbps) and allocated three `Vec`s per packet (FU-A payload, RTP packet,
interleaved frame; `malloc` 1.75 % self). Packetizing itself was 2.6 %.
Profile runs: before
[35438640329](https://github.com/Saul-Punybz/caudal/actions/runs/35438640329),
after [35438642844](https://github.com/Saul-Punybz/caudal/actions/runs/35438642844)
(artifacts hold `perf report` text and flamegraphs for both servers).
Caudal's total sampled cycles in the 28 s window fell from 23.9 G to 5.9 G;
MediaMTX's in the same runs were 25.4 G and 32.8 G (runner noise).

**Fix** (`crates/caudal-rtsp/src/rtp.rs`, `server.rs::play_task`): each
access unit is packetized straight into one per-session buffer, reused
across frames, with the `$` + channel + length prefix already in place.
TCP (and RTSPS) sends the whole access unit with one write; UDP sends the
same bytes minus the prefix, one datagram per packet. No per-packet
allocation. The buffer is dropped after a frame above 64 KB (a keyframe) so
idle viewers don't each hold a keyframe's worth of memory.

**Result**, median (min–max) of 3 repetitions, server CPU in % of one core,
same egress and zero RTP loss on both in every cell:

| Viewers | Build | Caudal CPU % | MediaMTX CPU % (same run) | Caudal / MediaMTX | Caudal RSS MB | MediaMTX RSS MB | Egress Mbps (both) |
|---|---|---|---|---|---|---|---|
| 100 | before (`main`) | 41.1 (40.5–43.6) | 42.0 (40.5–43.9) | 0.98 | 116 | 103 | 613–615 |
| 100 | after | 11.5 (11.5–11.5) | 63.0 (62.8–63.0) | 0.18 | 111 | 105 | 613–614 |
| 300 | before (`main`) | 120 (118–128) | 122 (121–125) | 0.98 | 150 | 124 | 1,838–1,843 |
| 300 | after | 29.9 (29.3–30.5) | 184 (182–186) | 0.16 | 131 | 124 | 1,842–1,843 |

Runs: before
[35438650106](https://github.com/Saul-Punybz/caudal/actions/runs/35438650106),
after [35438653990](https://github.com/Saul-Punybz/caudal/actions/runs/35438653990).
Two readings: on Linux the 18 Sep gap measured on the M4 was already a tie
(0.98), and after the fix Caudal uses about a sixth of MediaMTX's CPU for
the same delivery. The after run's MediaMTX numbers are higher than the
before run's (a different, busier runner), which is why the ratio, not the
absolute CPU, is the result; Caudal's own CPU also fell about 4x across
runs. RSS fell by about 20 MB at 300 viewers. Not re-measured: the M4, RTSP
over UDP under load (e2e decode only), and RTSP latency (one write per
frame sends the same packets at the same moment, so no change is
expected, but that is not measured).

### Latency

Re-measured 18 Sep 2026, 22:58–23:05 (`bench/results/20260918-225809.jsonl`,
`bench/run.sh --reps 3 --only latency --protos hls`), both servers in the
same session, 1-minute load average 3.5–5.3 from other work on the machine.

| Path | Server | Median ms | p95 ms | Samples | Per-run medians | PART-HOLD-BACK |
|---|---|---|---|---|---|---|
| FLV over TCP (no server, floor) | — | 10 | 13 | 60 | 8, 11, 11 | — |
| LL-HLS, live edge | Caudal | 212 | 224 | 60 | 208, 214, 213 | 0.601 s |
| LL-HLS, live edge | MediaMTX | 210 | 223 | 60 | 207, 211, 215 | 0.500 s |
| RTSP (TCP) | Caudal | 45 | 55 | 60 | 43, 51, 45 | — |
| RTSP (TCP) | MediaMTX | 45 | 56 | 60 | 42, 45, 51 | — |

- **LL-HLS: a tie at the live edge.** Both servers sit one part duration
  (200 ms) above the floor: the client fetches whole listed parts, so the
  first frame of a part waits for the rest of it. Neither server streams the
  preload-hinted part as it is written (checked on MediaMTX: the hinted
  part's response starts about 200 ms after the request and then arrives at
  once, same as Caudal).
- **Per part, the servers publish at the same moment.** Following each
  media playlist with blocking reloads and decoding every part: a part is
  listed 44 ms (Caudal) and 40 ms (MediaMTX) after the stamp of its last
  frame, and 210 / 207 ms after the stamp of its first (medians over about
  100 parts each, 6 frames per part on both).
- **The PART-HOLD-BACK difference remains**, about 0.1 s in a player:
  Caudal advertises three part targets (RFC 8216bis says it SHOULD be at
  least three, and Apple's validator warns below that, -50102); MediaMTX
  advertises 2.5.
- **Why the first run showed 379 vs 234 ms (and a 176 ms floor).** It was
  the decoder in the load client, not a server. ffmpeg 9 writes rawvideo at a
  constant frame rate by default and encodes it frame-threaded (one thread
  per core). When the input's start time is earlier than its first
  decodable frame, ffmpeg emits the gap as duplicate frames all at once; the
  frame-threaded encoder then returns at most one packet per new frame, so
  that burst stays queued for the whole session and every frame reaches the
  pipe several frames late. Caudal's playlist carries audio and video in one
  file, so its start time comes from both tracks and the burst happened
  every time; MediaMTX's video playlist has no audio track and it did not.
  Evidence: replaying the same recorded Caudal parts into ffmpeg at their
  recorded arrival times, frames 2–6 of each part came out about 200 ms
  after the part was written; with `-threads 16` about 400 ms; with
  `-threads 1` or `-fps_mode passthrough`, 3–7 ms. Publishing to Caudal
  without audio gave 184 ms with the old command, below MediaMTX. The same
  queue inflated the floor and RTSP numbers, which is why they drop from
  about 176 ms to 10 and 45 ms.

## NOT MEASURED, and why

- **A clean, idle machine.** Other processes used 1.3 to 3.3 cores during
  the windows; see Setup. Re-run `bench/run.sh` on a quiet machine before
  quoting absolute numbers.
- **RTSP and WHEP capacity at 1,000 viewers.** Machine-limited (kernel
  network memory for RTSP; for WHEP the one-process load client and UDP
  socket buffers). A second machine for the load client is the fix.
- **Glass-to-glass as a person sees it.** Measured from the moment a frame
  leaves the source's filter graph to decoded frame at the viewer; camera
  capture and display are not included. LL-HLS is edge latency (no player
  hold-back); the player figure above is an estimate, not a measurement.
- **WHEP latency.** No headless WebRTC decoder in the harness; not
  measured.
- **RTSP over UDP**, **RTMP or SRT playback**, **MoQ**: not in scope.
- **Egress measured on the server side.** Egress is what the client
  received (payload bytes), not bytes on the wire.
- **MediaMTX packaging on first request** (`hlsAlwaysRemux: false`, its
  default) was not measured; both servers package from publish time.

## Conclusion

Wins for Caudal: binary 2.5x smaller (21.8 vs 54.1 MB), idle memory 2.5x
lower (14 vs 35 MB), LL-HLS fan-out at about half the CPU per gigabit
with less memory, zero errors on both, up to 1,000 viewers, and (after the
19 Sep send-path fix, on Linux) RTSP fan-out at about a sixth of MediaMTX's
CPU for the same egress.

Losses for Caudal: more memory per live stream (98 vs 80 MB; 87 MB with an
equal 14 s buffer), and more CPU for
WHEP at 300 viewers (315 vs 178 %) although it now keeps every viewer up
where MediaMTX does not (see "WHEP after the fixes"; the original WHEP
collapse is fixed).

Ties: CPU with one publisher, LL-HLS edge latency, RTSP latency, idle CPU. Machine-limited on
both: RTSP at 1,000 viewers.

Do not claim "faster than MediaMTX" in general. Claims these numbers
support: "a smaller binary and lower idle memory than MediaMTX" and "LL-HLS
fan-out at about half MediaMTX's CPU on the same machine". Next fixes that
the benchmark points to: WHEP CPU per packet (allocation, batched sends)
and memory per live stream. Also supported now, on Linux: "RTSP fan-out
at a fraction of MediaMTX's CPU" (see "RTSP after the send-path fix"). Also supported now: "WHEP
keeps every viewer up at 300 where MediaMTX does not, with less memory".
