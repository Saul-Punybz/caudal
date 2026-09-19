# Caudal vs MediaMTX v1.21: side-by-side benchmark

Measured 18 Sep 2026 on one machine, one server at a time, release builds,
same input. Reproduce with one command: `bench/run.sh` (about 80 minutes;
options pass through to `bench/bench.py run`, e.g.
`bench/run.sh --reps 1 --levels 1,100 --protos hls`). Raw data:
`bench/results/20260918-202735.jsonl` (main run) and
`bench/results/20260918-214620.jsonl` (buffer sensitivity run), with the
generated tables next to them (`.md`).

## Short answer

| Metric | Caudal | MediaMTX | Verdict |
|---|---|---|---|
| Binary size | 21.8 MB | 54.1 MB | **Caudal 2.5x smaller** |
| Idle RSS | 14.0 MB | 35.4 MB | **Caudal 2.5x less** |
| RSS with 1 publisher (1080p30, 6 Mbps) | 97.9 MB (86.8 MB with a 14 s buffer) | 80.3 MB | **MediaMTX lighter** |
| CPU with 1 publisher | 1.0 % | 1.5 % | tie (both negligible) |
| LL-HLS, 1,000 viewers: server CPU | 83 % | 168 % | **Caudal about 2x less CPU** |
| LL-HLS, 1,000 viewers: server RSS | 215 MB | 339 MB | **Caudal less** |
| LL-HLS, 100 / 300 viewers: server CPU | 8.7 % / 26.5 % | 26.7 % / 72.4 % | **Caudal about 3x less** |
| RTSP (TCP), 100 / 300 viewers: server CPU | 122 % / 319 % | 104 % / 278 % | **MediaMTX about 15 % less** |
| RTSP, 1,000 viewers | 14 of 1,000 kept up | 0 of 1,000 kept up | neither works here; the machine's network memory is the limit (see below) |
| WHEP, 100 viewers: CPU / RSS | 87 % / 334 MB | 73 % / 524 MB | **MediaMTX less CPU, Caudal less memory** |
| WHEP, 300 viewers | **245 Mbps delivered, 0 of 300 kept up** | 1,377 Mbps delivered, 0 of 300 kept up | **MediaMTX delivers 5.6x more; Caudal's WHEP falls apart between 100 and 300 viewers** |
| WHEP, 1,000 viewers | 307 Mbps, sessions failing (1,116 timeouts) | 1,269 Mbps, no errors, 4.4 GB RSS | **MediaMTX better** (neither keeps up) |
| LL-HLS latency, live edge (median / p95) | 379 / 445 ms | 234 / 249 ms | **MediaMTX about 145 ms lower** |
| RTSP latency (median / p95) | 177 / 381 ms | 275 / 312 ms | tie within this method's noise |

"Faster" is only supported for **LL-HLS fan-out CPU and memory**, plus binary
size and idle memory. On RTSP fan-out, WHEP, LL-HLS latency and memory with
a live stream, MediaMTX is equal or better today.

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
  same.
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

### Latency

| Path | Server | Median ms | p95 ms | Samples | Per-run medians | PART-HOLD-BACK |
|---|---|---|---|---|---|---|
| FLV over TCP (no server, floor) | — | 176 | 215 | 60 | 210, 176, 109 | — |
| LL-HLS, live edge | Caudal | 379 | 445 | 60 | 380, 441, 354 | 0.601 s |
| LL-HLS, live edge | MediaMTX | 234 | 249 | 60 | 234, 214, 244 | 0.500 s |
| RTSP (TCP) | Caudal | 177 | 381 | 60 | 177, 143, 377 | — |
| RTSP (TCP) | MediaMTX | 275 | 312 | 60 | 176, 275, 310 | — |

- **LL-HLS: MediaMTX is about 145 ms lower at the live edge**, in all three
  runs (Caudal's best run, 354 ms, is above MediaMTX's worst, 244 ms). Adding
  each server's PART-HOLD-BACK gives a rough player estimate of about
  0.98 s for Caudal and 0.73 s for MediaMTX. Both are well under the 3 s
  goal in `STATUS.md`; the gap is still Caudal's to close (Caudal's edge
  latency is about 200 ms, one part duration, above the no-server floor;
  MediaMTX's is about 60 ms above it).
- **RTSP: a tie within noise.** Per-run medians swing by 150–230 ms on both
  servers, and the no-server baseline itself swings 109–210 ms between
  runs, so the method (ffmpeg decode start and pacing on a shared machine)
  has more run-to-run noise than the difference being measured.

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
lower (14 vs 35 MB), and LL-HLS fan-out at about half the CPU per gigabit
with less memory, zero errors on both, up to 1,000 viewers.

Losses for Caudal: more memory per live stream (98 vs 80 MB; 87 MB with an
equal 14 s buffer), about 15 % more CPU for RTSP fan-out, about 16 % more CPU
for WHEP at 100 viewers, WHEP delivery that collapses between 100 and 300
viewers without being CPU-bound, and about 145 ms more LL-HLS edge latency.

Ties: CPU with one publisher, RTSP latency, idle CPU. Machine-limited on
both: RTSP at 1,000 viewers.

Do not claim "faster than MediaMTX" in general. Claims these numbers
support: "a smaller binary and lower idle memory than MediaMTX" and "LL-HLS
fan-out at about half MediaMTX's CPU on the same machine". Next fixes that
the benchmark points to: the WHEP egress path at 300 or more peers, LL-HLS
part publication latency, and RTSP send-path CPU.
