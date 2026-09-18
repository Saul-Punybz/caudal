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
| SRT | `srt-tokio` | 0.4.4 | Apache-2.0 | github.com/rosalyntg/srt-rs | May 2024 | trial; calls itself "not production ready" |
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
| SRT (if `srt-tokio` falls short) | github.com/datarhei/gosrt | active, readable, spec-complete |
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

## What nobody has built: we write it

- LL-HLS packager (on top of `mp4-atom` + `m3u8-rs`)
- RTSP server (ported from gortsplib)
- JWKS fetch-and-cache (small, on top of `jsonwebtoken`)
- MistServer config import
- The live buffer: **already written** (`caudal-core`). We keep it and align its vocabulary with MoQ's track/group model so MoQ output is direct.

## Checked and rejected

- **Forking xiu:** stale, and retrofitting CMAF, LL-HLS and MoQ onto its FLV model costs more than building on crates.
- `moq-karp`: dead since Mar 2025, replaced by `hang`.
- `moq-lite` by name: renamed to `moq-net`.
- `foca`: MPL-2.0; `chitchat` does the same job under MIT.
- `jwks-client`: unmaintained.
- Rewriting in Go: MediaMTX (MIT, ★20K) already exists there; see PLAN.md.
