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
TypeScript + Tailwind + shadcn/ui, with uPlot charts, live updates over
WebSocket/SSE and an OpenAPI-generated client, embedded in the binary with
`rust-embed`. Same screens (overview, streams, protocols, push, triggers,
logs, stats, keys, embed, preview), redesigned.

## Libraries (reuse before writing)

Full list with repos and licenses: [REUSE.md](REUSE.md).

Picked from a crates.io survey on 17 Sep 2026. All MIT or Apache-2.0 except where noted.

| Area | Crate | Note |
|---|---|---|
| RTMP | `scuffle-rtmp` + `scuffle-flv` + `scuffle-transmuxer` (fallback `rml_rtmp`) | shortest path to fMP4 |
| SRT | `srt-tokio` 0.4 | pure Rust but says "not production ready". Risk: fall back to libsrt over FFI behind a feature flag |
| WebRTC / WHIP / WHEP | `str0m` 0.23 | sans-I/O |
| MP4 / CMAF | `mp4-atom` 0.15 | |
| MPEG-TS | `mpeg2ts` 0.6 | |
| HLS playlists | `m3u8-rs` 6 | |
| H.264 / HEVC / AV1 | `h264-reader`, `scuffle-h265`, `scuffle-av1` | AV1 parsing is young |
| MoQ | `moq-net` (was `moq-lite`), `hang`, `moq-relay`, `quinn` | |
| HTTP / TLS / ACME | `axum`, `rustls`, `rustls-acme` | |
| Metrics | `metrics` + `metrics-exporter-prometheus` | |
| RTSP client | `retina` | no Rust RTSP **server** exists; we write it |
| RIST | `librist-sys` (BSD-2, C) | no pure-Rust RIST exists; feature flag |

Reference implementations to learn from: `xiu` (MIT), the `scuffle` crates (MIT/Apache).

## Milestones

- [x] **M0: Core.** Media model and live ring buffer (`caudal-core`).
- [ ] **M1: Server shell.** `caudal` binary, TOML config, HTTP API, `/metrics`, graceful shutdown.
- [ ] **M2: RTMP ingest.** OBS or ffmpeg publishing into the ring.
- [ ] **M3: LL-HLS / CMAF output.** First playable path: OBS → Caudal → browser.
- [ ] **M4: SRT** in and out.
- [ ] **M5: WebRTC.** WHIP in, WHEP out.
- [ ] **M6: MoQ / WebTransport.**
- [ ] **M7: Auth + TLS.** JWT/JWKS, webhooks, ACME.
- [ ] **M8: Recording and VOD.** MP4 to disk, DVR window, MP4/TS/MKV file inputs.
- [ ] **M9: RTSP** pull (retina) and server.
- [ ] **M10: Clustering.** Origin-edge.
- [ ] **M11: Transcoding.** External ffmpeg process, never linked, so no GPL.

MistServer outputs that are dead in 2026 (HDS, Flash, Smooth Streaming) are
not being ported.

## Rules

- Measured, not claimed: every "faster / smaller" claim comes with a benchmark against MistServer on the same machine.
- Every parser gets a fuzz target before it touches the network.
- `unsafe` only inside FFI crates, never in `caudal-core`.
