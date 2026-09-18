# What Caudal reuses

Checked 17 Sep 2026 against crates.io and the GitHub API: versions,
licenses and last activity are real, not remembered. Rule: depend on
it if it exists and the license allows; port it if it only exists in
another language; write it only if nothing exists.

## Dependencies (Rust crates)

| Area | Crate | Version | License | Repo | Last activity | How we use it |
|---|---|---|---|---|---|---|
| RTMP ingest | `scuffle-rtmp` | 0.2.3 | MIT OR Apache-2.0 | github.com/ScuffleCloud/scuffle | crate May 2025 | dependency |
| FLV parsing | `scuffle-flv` | 0.2.2 | MIT OR Apache-2.0 | same | May 2025 | dependency |
| FLV → fMP4 | `scuffle-transmuxer` | 0.2.2 | MIT OR Apache-2.0 | same | May 2025 | dependency: the shortest RTMP → CMAF path |
| H.264 / H.265 / AV1 | `scuffle-h264`, `-h265`, `-av1` | 0.2.2 / 0.2.2 / 0.1.4 | MIT OR Apache-2.0 | same | May 2025 | dependency |
| fMP4 / CMAF | `mp4-atom` | 0.15.0 | MIT OR Apache-2.0 | github.com/kixelated/mp4-atom | Sep 2026 | dependency for the muxer |
| HLS playlists | `m3u8-rs` | 6.0.1 | MIT | github.com/rutgersc/m3u8-rs | Jul 2026 | dependency; LL-HLS tags (`EXT-X-PART`, preload hints) added by us |
| WebRTC / WHIP / WHEP | `str0m` | 0.23.1 | MIT OR Apache-2.0 | github.com/algesten/str0m | Aug 2026 | dependency |
| MoQ transport | `moq-net` (was `moq-lite`) | 0.2.22 | MIT OR Apache-2.0 | github.com/moq-dev/moq | Sep 2026 | dependency |
| MoQ media | `hang` | 0.20.13 | MIT OR Apache-2.0 | same | Sep 2026 | dependency |
| MoQ relay + clustering | `moq-relay` | 0.14.18 | MIT OR Apache-2.0 | same | Sep 2026 | dependency or reference; already does cluster mesh and path-scoped JWT |
| SRT | `rsrt` | 0.3.6 | Apache-2.0 | github.com/cesbo/rsrt | Sep 2026 | dependency. Pure Rust, tokio, HaiCrypt AES. Verified 17 Sep 2026: 668 tests pass incl. 101 interop tests against libsrt 1.5.6 in both directions; manual libsrt→rsrt: 700 pkts, 0 lost. Fallbacks: `shiguredo_srt` (sans-I/O, Apache-2.0), `srt-tokio` (stale 2024) |
| RIST | `rist-core` + `rist-mio` | 0.1.0 | MIT | github.com/wavey-ai/rist-rs | Aug 2026 | trial. Pure-Rust sans-I/O engine with a C-parity checklist: Simple + Main profiles, SRP/PSK, NACK/recovery, IPv4/6 done; multicast, multipath, Advanced profile not. 210 tests, interop suite vs librist. Repo also ships `rist-sys` (C bindings): not used |
| RTSP client (pull) | `retina` | 0.4.20 | MIT/Apache-2.0 | github.com/scottlamb/retina | Aug 2026 | dependency |
| JWT | `jsonwebtoken` | 11.1.0 | MIT | github.com/Keats/jsonwebtoken | Sep 2026 | dependency |
| Signed webhooks | `standardwebhooks` | 1.0.1 | MIT | github.com/standard-webhooks/standard-webhooks | Sep 2026 | dependency |
| Automatic HTTPS | `rustls-acme` | 0.15.4 | Apache-2.0 OR MIT | github.com/FlorianUekermann/rustls-acme | Jul 2026 | dependency |
| Cluster membership | `chitchat` | 0.13.0 | MIT | github.com/quickwit-oss/chitchat | Sep 2026 | dependency (gossip: who is alive) |
| Stream placement | `hashring` | 0.3.6 | MIT | github.com/jeromefroe/hashring-rs | Aug 2024 | dependency (which node owns stream X) |
| API contract | `utoipa` | 5.5.0 | MIT OR Apache-2.0 | github.com/juhaku/utoipa | Sep 2026 | OpenAPI → generated TypeScript client |
| UI inside the binary | `rust-embed` | 8.12.0 | MIT | pyrossh.dev/repos/rust-embed | Jul 2026 | dependency |
| HTTP / TLS / QUIC | `axum`, `hyper`, `rustls`, `quinn` | current | MIT / Apache | tokio-rs, rustls, quinn-rs | active | dependency |

## Ported from Go (read, then write in Rust)

No usable Rust version exists for these. All three Go sources are MIT, so porting is allowed.

| What | Go source | Why |
|---|---|---|
| RTSP **server** | github.com/bluenviron/gortsplib | Rust only has clients (`retina`) |
| LL-HLS server behavior | github.com/bluenviron/gohlslib + mediamtx's HLS muxer | blocking playlist reload, parts, preload hints: no Rust crate does this turnkey |

## Reference only (read, don't depend)

| Repo | License | For what |
|---|---|---|
| github.com/harlanc/xiu | MIT | Rust media server; its `streamhub` and RTMP edge cases. Not a fork base: stale since Mar 2026 and built around FLV, not CMAF/MoQ |
| github.com/KallDrexx/rust-media-libs (`rml_rtmp`) | MIT | fallback RTMP if scuffle falls short; stale since 2023 |
| GStreamer gst-plugins-rs `hlssink3` | MPL-2.0 + LGPL | LL-HLS/CMAF design; never linked |
| DDVTECH/mistserver | Unlicense | behavior, config and API to stay compatible with |

## UI (web, embedded in the binary)

| Piece | Project | License |
|---|---|---|
| Framework | React + Vite + TypeScript | MIT |
| Components | shadcn/ui (github.com/shadcn-ui/ui) + Tailwind | MIT |
| Live charts | uPlot (github.com/leeoniya/uPlot) | MIT |
| HLS player | hls.js (github.com/video-dev/hls.js) | Apache-2.0 |
| MoQ player | `@moq/watch` (from moq-dev/moq) | MIT OR Apache-2.0 |
| WebRTC player | the browser's native WebRTC (WHEP) | — |

## What doesn't exist in Rust: we write it in Rust

Decision (17 Sep 2026): everything in Rust. No C or Go linked. The Go and C
sources above are read as specifications only.

| # | Piece | Why we write it | Size |
|---|---|---|---|
| 1 | **LL-HLS packager** (parts, blocking reload, preload hints) | no Rust crate does the server side | large |
| 2 | **RTSP server** | Rust only has clients (`retina`); port from gortsplib | large |
| 3 | **RIST gaps** | `rist-core` covers Simple + Main; we add what its checklist marks ❌ if we need it (multicast, multipath) | medium |
| 4 | **SRT gaps** | `rsrt` covers live mode; out of scope upstream: rendezvous, FEC filter, AES-GCM, bonding, IPv6. Add IPv6 first | small–medium |
| 5 | **Clustering for RTMP/HLS/SRT/WebRTC** | `moq-relay` only clusters MoQ; glue on chitchat + hashring | medium |
| 6 | **MistServer config import** | nobody has it | small |
| 7 | **JWKS cache** | existing crates are stale | small |
| 8 | **ACME cert shared with QUIC** | rustls-acme only covers HTTP | small |
| 9 | **LL-HLS tags in m3u8-rs** | missing; contribute upstream | small |
| 10 | **Opus header parsing, AV1 gaps** | only a 2020 crate / young crate | small |
| — | The live buffer | **already written** (`caudal-core`) | done |

**The one honest exception: transcoding.** There is no production-grade
pure-Rust H.264/HEVC/AAC encoder. `rav1e` (AV1, BSD-2) is the only serious
Rust encoder; `openh264` is C bindings; `less-avc` is intra-only. So:
AV1 transcoding in Rust via `rav1e`, and everything else via an ffmpeg
**external process**, never linked. Caudal itself stays 100% Rust.

## Checked and rejected

- `srt-tokio`: superseded by `rsrt` (verified against libsrt).
- `jonasohland/rist-rs`: no license, dead since Jan 2024.
- `fishloa/rust-broadcast` (`rist-runtime`): 476K lines, one author, only RTCP message types for RIST; too big and too thin at once.

- **Forking xiu:** stale, and retrofitting CMAF, LL-HLS and MoQ onto its FLV model costs more than building on crates.
- `moq-karp`: dead since Mar 2025, replaced by `hang`.
- `moq-lite` by name: renamed to `moq-net`.
- `foca`: MPL-2.0; `chitchat` does the same job under MIT.
- `jwks-client`: unmaintained.
- `librist-sys` (C bindings): replaced by a pure-Rust RIST port.
- Rewriting in Go: MediaMTX (MIT, ★20K) already exists there; see PLAN.md.
