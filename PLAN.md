# Caudal — plan

Open-source media server in Rust. A rewrite of MistServer
(github.com/DDVTECH/mistserver, C++, ~187K lines, Unlicense) that fixes what
its users keep reporting and adds what it is missing in 2026.

## Why rewrite instead of patch

MistServer's recurring bugs come from its design, not from typos:

| Problem in MistServer | Evidence | How Caudal removes it |
|---|---|---|
| Memory-corruption crashes in hand-written parsers | #151, #150, #148 segfaults; #252 buffer overread in the controller | Safe Rust; parsers fuzzed in CI |
| Global-mutex deadlocks and a controller stuck at 100% CPU | #119 (6,857 threads deadlocked on `configMutex`), #258 | Config is an immutable snapshot swapped atomically; no global lock on the hot path |
| One process per input/output, shared-memory pages | architecture | One async process; frames are shared by reference, not copied |
| A slow viewer can hold up memory | architecture | The ring never waits on a viewer: slow viewers skip to a keyframe (done: `caudal-core`) |
| Timestamps rounded to ms (audio drift) | `lib/dtsc.h` | Native timescale per track (done: `caudal-core`) |
| Hand-rolled crypto, JSON, HTTP, JWT | `lib/rijndael.cpp`, `json.cpp`, `http_parser.cpp`, `jwt.cpp` | rustls, serde, hyper/axum, jsonwebtoken |
| CMAF/DASH broken or wrong track | #281, #297, #225, #212 | New CMAF muxer on `mp4-atom`, tested against the spec and real players |
| TS-based HLS saturates CPU (700 viewers, 2 Gbps, EPYC) | #280 (open) | Segments are built once per rendition and served as shared bytes |
| Docker builds compiling 5 C libraries; Alpine ABI bugs | #229, #217, #270 | One static binary, `FROM scratch` image |
| Config not persisting | #232, #274, #199, #167 | One TOML file, atomic write, validated before it is applied |

## What Caudal adds that MistServer doesn't have

1. **Media over QUIC (MoQ) + WebTransport**: requested in #292 and #288 and never open-sourced upstream. Built on `moq-net` / `hang`.
2. **Built-in ACME**: automatic Let's Encrypt certificates via `rustls-acme`. Upstream only proxies the HTTP-01 challenge to certbot.
3. **Origin-edge clustering / load balancing**: requested in #190, open since 2023.
4. **JWKS / OIDC auth** for publish, play and webhooks: #207, #275.
5. **WebRTC that works with AV1, Opus and Firefox**: #242, #251, #286, #257.
6. **LL-HLS on by default**: #166.
7. **OpenTelemetry traces** alongside Prometheus metrics.
8. **MistServer config import**: read an existing `mistserver.conf` so people can migrate.

## Why Rust and not Go

Measured on an M-series Mac, 17 Sep 2026:

| | Go: MediaMTX 1.21 | Rust: tokio + axum baseline |
|---|---|---|
| Binary | 54 MB | 0.5 MB (full Caudal expected at 10–20 MB, to be measured) |
| Idle RSS | 38 MB | 6.5 MB |

Go's media ecosystem is more mature (pion, gortsplib, gosrt, mp4ff), but a
Go rewrite would be a second MediaMTX (MIT, ★20K). Rust gives predictable
memory with no GC pauses at multi-Gbps fan-out, and the compiler rejects
data races, the class behind MistServer's deadlocks (#119). Where Rust has
gaps (RTSP server, SRT maturity) the MIT Go code is ported, not linked.

## UI

MistServer's UI is one ~21K-line jQuery file. Caudal ships React + Vite +
TypeScript + Tailwind built to **Material Design 3** (m3.material.io), with
color tokens generated from the brand palette (Orange `#F54F1B`, Space Cadet
`#1E223D`, Gargoyle Gas `#E6D5B7`) by Google's material-color-utilities. See
`ui/DESIGN.md`. uPlot charts, live updates over WebSocket/SSE, an
OpenAPI-generated client, embedded in the binary with `rust-embed`. Same screens (overview, streams, protocols, push, triggers,
logs, stats, keys, embed, preview), redesigned.

## Libraries (reuse before writing)

Full list with repos and licenses: [REUSE.md](REUSE.md).

Picked from a crates.io survey on 17 Sep 2026. All MIT or Apache-2.0 except where noted.

| Area | Crate | Note |
|---|---|---|
| RTMP | `scuffle-rtmp` + `scuffle-flv` + `scuffle-transmuxer` (fallback `rml_rtmp`) | shortest path to fMP4 |
| SRT | `srt-tokio` 0.4 | pure Rust but says "not production ready". Risk: finish it upstream or port gosrt, in Rust |
| WebRTC / WHIP / WHEP | `str0m` 0.23 | sans-I/O |
| MP4 / CMAF | `mp4-atom` 0.15 | |
| MPEG-TS | `mpeg2ts` 0.6 | |
| HLS playlists | `m3u8-rs` 6 | |
| H.264 / HEVC / AV1 | `h264-reader`, `scuffle-h265`, `scuffle-av1` | AV1 parsing is young |
| MoQ | `moq-net` (was `moq-lite`), `hang`, `moq-relay`, `quinn` | |
| HTTP / TLS / ACME | `axum`, `rustls`, `rustls-acme` | |
| Metrics | `metrics` + `metrics-exporter-prometheus` | |
| RTSP client | `retina` | no Rust RTSP **server** exists; we write it |
| RIST | written in Rust (ported from libRIST) | no pure-Rust RIST exists |

Reference implementations to learn from: `xiu` (MIT), the `scuffle` crates (MIT/Apache).

## Milestones

- [x] **M0: Core.** Media model and live ring buffer (`caudal-core`).
> **Survey result, 17 Sep 2026 (see REUSE.md, backlog tiers):** moq-dev/moq already
> ships the gateway family we planned to write (`moq-mux`, `moq-hls`, `moq-rtmp`
> with enhanced RTMP, `moq-srt`, `moq-relay` with clustering and JWT). Decision for
> M1: evaluate building Caudal's protocol layer on top of `hang` + `moq-mux`
> before writing our own packagers. If it holds, M3 and M6 shrink to integration
> work and `caudal-core` becomes the DVR/failover layer around a hang broadcast.

- [x] **M1: Server shell.** `caudal` binary, TOML config, HTTP API, `/metrics`, graceful shutdown.
- [x] **M2: RTMP ingest.** OBS or ffmpeg publishing into the ring.
- [x] **M3: LL-HLS / CMAF output.** (browser playback still to confirm) First playable path: OBS → Caudal → browser.
- [x] **M4: SRT** in (batch 2) and out: pull + push (batch 6).
- [x] **M5: WebRTC.** WHIP in, WHEP out (no TURN yet; AAC sources video-only until M11).
- [x] **M6: MoQ / WebTransport.** Output done (ingest over MoQ still to do).
- [x] **M7: Auth + TLS.** JWT/JWKS, webhooks, ACME (ACME untested against a real CA; Safari over h2 pending a trusted local cert).
- [x] **M8: Recording and VOD.** (batch 6; S3/GCS upload untested against real buckets) MP4 to disk, DVR window, MP4/TS/MKV file inputs.
- [ ] **M9: RTSP** pull (retina) and server.
- [x] **M10: Clustering.** Origin-edge: an edge pulls a stream from its origins over MoQ on the first viewer (any protocol), shares one pull, stops when idle, and fails over between origins without restarting its viewers (`caudal-cluster`, `docs/research/CLUSTER.md`). Our own MoQ client, not `moq-relay`: a pulled stream has to land in the edge's registry so LL-HLS/WHEP/RTSP/SRT serve it.
- [x] **M11: Transcoding.** Ladders as `<name>+<label>` via external ffmpeg (default) or in-process rusty_h264 (works; 2–3x the CPU of x264, ~4.7 dB lower PSNR at the same bitrate, batch 7).
- [ ] **M13: 24/7 channels from files** (decided by Saul, 18 Sep 2026: Caudal includes playout, all in Rust, no concern about overlapping ANTENA787). Playlists and schedules of MP4/TS/MKV files published as a continuous live stream, with transitions on keyframes and timestamp stitching; normalization through the transcoder when files differ.
- [ ] **M12: Open Media Transport (OMT), the open NDI alternative.** Pure-Rust port of the MIT reference (libomtnet C#, ~10.6K lines) and the VMX codec (libvmx C, ~23.5K lines incl. SIMD), mDNS discovery with `mdns-sd`. First native Rust OMT. Send and receive, so Caudal can take in and put out LAN video for live production.
  - **Licensing (changed by Saul, 18 Sep 2026): MIT OR Apache-2.0**, like the rest of Caudal and like OMT's own reference code. Goal: the definitive, go-to OMT implementation in Rust, adopted widely. Replaces the earlier PolyForm Noncommercial decision the same day.
  - **Standalone crates, not buried in Caudal**, so any Rust project can use them: `open-media-transport` (protocol, send/receive, discovery) and `vmx-codec` (the VMX codec), both free on crates.io as of 18 Sep 2026. Own repo; Caudal depends on them like any other crate (in the default build, since the license no longer restricts it).
  - Copyright: Saul González and Puny.bz Inc. (co-ownership agreement signed; the switch to MIT OR Apache-2.0 verified with legal, confirmed by Saul 18 Sep 2026). Keep the MIT notices of `libomtnet` and `libvmx` for ported parts, crediting their authors. Contributions under the usual Apache-2.0 inbound = outbound terms (DCO sign-off), no CLA needed.
  - Name: the crates may say what they implement ("Open Media Transport"); don't use OMT's logos as our branding.

MistServer outputs that are dead in 2026 (HDS, Flash, Smooth Streaming) are
not being ported.

## Rules

- Measured, not claimed: every "faster / smaller" claim comes with a benchmark against MistServer on the same machine.
- Every parser gets a fuzz target before it touches the network.
- 100% Rust: no C or Go linked. `unsafe` needs a written justification and never appears in `caudal-core`.

## Backlog: beyond parity (added 17 Sep 2026)

Ranked by value for a small team running live video in 2026. "Nobody" means
neither MistServer nor MediaMTX ships it, as far as checked; verify before
claiming in public.

### A. Table stakes that MistServer lacks
- **Enhanced RTMP** (HEVC/AV1 over RTMP, what OBS and YouTube use since 2023). Check `scuffle-rtmp` support first (unverified).
- **WHIP ingest from OBS 30+** and browsers, not only WHEP playback.
- **SRT stream-id routing** (one port, many streams, stream key in the id) and **SRTLA bonding** for IRL streaming (`srtla-rs` exists, MIT, verify).
- **Backup source / failover**: a stream declares a primary and a backup input; viewers never see the switch.
- **Hot config reload** with validation (`caudal check config.toml`), atomic apply, no restart.
- **Structured JSON logs**, OpenTelemetry traces, `/healthz` + `/readyz`, graceful drain on shutdown.
- **Timed metadata**: SCTE-35 → `EXT-X-DATERANGE`, ID3 in fMP4 (`emsg`), CEA-608/708 caption passthrough, WebVTT subtitles, multi-audio.
- **Recording to object storage** (S3/R2/GCS via `object_store`), segment upload as they close, HLS VOD from recordings.
- **Clip export by time range**: `POST /streams/x/clips {from,to}` → MP4. Uses the DVR window.
- **Thumbnails / preview sprites** via external ffmpeg (no pure-Rust H.264 decoder that is production grade).
- **Static builds for arm64** (Raspberry Pi, Ampere, Apple Silicon) in CI, plus Helm chart and a Kubernetes example.

### B. Differentiators nobody has
- **WASM plugins** (wasmtime): triggers, auth decisions and frame-level filters run in a sandbox at native speed. MistServer's triggers are shell scripts; this replaces them safely.
- **End-to-end latency measurement**: the server stamps wall-clock into `emsg`/SEI; the shipped player reports glass-to-glass latency and rebuffering back to `/beacon`. The UI shows real QoE per viewer, not just bytes sent.
- **Built-in network impairment** for testing (`caudal lab --loss 3% --jitter 40ms`): reproduce field problems on a laptop.
- **MoQ-first fan-out**: every stream is a MoQ broadcast internally, so HLS/WebRTC/SRT are views of it. Clustering comes from `moq-relay` instead of a second mechanism.
- **Multi-tenancy**: namespaces with quotas (viewers, Mbps, storage) and per-tenant API keys. Needed to sell hosting on top of it.
- **Declarative streams** (GitOps): the config file is the source of truth; the UI edits it and commits, not the other way around.
- **`caudal doctor`**: checks ports, NAT, TLS, clock, codecs of an incoming stream, and prints what to fix.
- **Conformance in CI**: Apple `mediastreamvalidator` on every LL-HLS change, fuzz targets on every parser, interop tests against libsrt/librist/ffmpeg/OBS.

### A+. From the competition study (18 Sep 2026, docs/research/COMPETITION.md)
- **Automatic live captions** (local speech-to-text → WebVTT in LL-HLS, CEA-608 in TS; es + en). ADA Title II: 26 Apr 2027 / 26 Apr 2028.
- **Multistreaming** to YouTube/Twitch/Facebook (RTMP push with per-destination status).
- **Admin login** for UI and API (OIDC/SSO; LDAP optional). Blocker for public deployments.
- **Stream health alerts** (no keyframes, bitrate drop, publisher lost) → webhooks / email / Slack.
- **Recording schedules.**
- **MoQ on Safari 26.4+** (WebTransport shipped); test and lift the UI gate.
- **Geo-blocking / IP allow-lists** per stream.
- **Raspberry Pi appliance image.**
- **DASH output** (low priority).
- **SCTE-35 passthrough** moved up (FAST channels; Gemini cross-check).
- **Forensic watermarking** (per-viewer marks; needs transcoding).
- **Player SDKs** (web component first, then mobile).
- **OBS guide**: OBS 30+ publishes WHIP natively; add "copy WHIP URL + token" to the UI.

### C. Later
- **Hardware transcoding** (Saul, 19 Sep 2026: good to have, after Caudal is complete): `[transcode] hw = "auto"` probes VideoToolbox / NVENC / VAAPI / QSV / V4L2 M2M with a test encode and falls back to libx264 (today hardcoded, `crates/caudal-transcode/src/ffmpeg.rs:224`). Stays in the ffmpeg child process so a driver crash can't take the server down. Verify with ffprobe and a max-simultaneous-ladders count, GPU vs CPU.
- **WebRTC across cores** (same decision): the engine is one task on one UDP socket for every peer (`crates/caudal-webrtc/src/lib.rs:89`), so all DTLS/SRTP work runs on one core. Shard per core (N sockets or SO_REUSEPORT); verify with the WHEP load from the MediaMTX benchmark (CPU spread across cores).
- HDR metadata passthrough (HEVC SEI via `hevc_parser`), Dolby Vision RPU.
- Scheduled/playlist channels from VOD files (keep small; ANTENA787 is the real playout).
- NDI itself: proprietary SDK, license restricts reverse engineering, trademarked. Covered instead by OMT (M12), an MIT protocol for the same job.
- Content protection: CENC/ClearKey first, Widevine/FairPlay only with a real customer.

## Road to "finished" (Saul, 18 Sep 2026: finish Caudal with everything, all in Rust)

| Batch | Work | Agent / model | State |
|---|---|---|---|
| 7 | RTSP pull + server (U), OMT vmx-codec (X, separate repo) | Sonnet / Opus | running |
| 8 | M13 24/7 channel from files, `caudal-channel` (Y) | Opus | running |
| 8 | Multistreaming RTMP/RTMPS push, `caudal-restream` (Z) | Sonnet | running |
| 9 | SCTE-35 passthrough → `EXT-X-DATERANGE` (`scte35-splice`); channel ad-break markers | Sonnet | next |
| 9 | Admin login (OIDC + local password) for UI and API | Opus | next |
| 9 | Stream health alerts (no keyframes, bitrate drop, publisher gone) → webhooks | Haiku/Sonnet | next |
| 9 | **Close the gap with MediaMTX (Saul, 18 Sep 2026):** (1) RTSP over UDP + RTSPS in `caudal-rtsp` (today TCP only, UDP answers 461); (2) hot config reload without dropping viewers (SIGHUP + `POST /api/v1/config/reload`; diff sections, restart only what changed); (3) side-by-side benchmark vs MediaMTX v1.21 on the same machine: CPU, RSS, binary size, glass-to-glass latency at 1/100/1,000 viewers over LL-HLS, WHEP, RTSP; script in `bench/`, results in `docs/research/BENCH-MEDIAMTX.md`. No "faster" claim until (3) exists. | Sonnet (1, 2), Opus (3) | next |
| 10 | M10 origin-edge clustering on `moq-relay` cluster (xiu as RTMP reference), failover/backup source | Opus | planned |
| 10 | Live captions (Whisper-class, es/en) → WebVTT in LL-HLS: `caudal-captions` (candle, pure Rust), `[captions]`; design + measured RTF in docs/research/CAPTIONS.md. CEA-608 in TS not built | Opus | done (feat/live-captions) |
| 11 | Recording schedules, geo-blocking/IP lists, DASH, MoQ ingest, MoQ on Safari 26.4+ | Sonnet | planned |
| 12 | Helm chart, Raspberry Pi image, `caudal doctor`, hot config reload, MistServer config import | Haiku/Sonnet | planned |
| 12 | OMT protocol + discovery (`open-media-transport`), player SDKs, watermarking (visible first, then A/B segment forensic) | Opus/Sonnet | planned |
