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
| MoQ relay + clustering | `moq-relay` | 0.14.18 | MIT OR Apache-2.0 | same | Sep 2026 | **embeddable library** as well as a binary (`rs/moq-relay/src/lib.rs`: `Relay::load` / `Relay::run`); cluster mesh and path-scoped JWT |
| SRT | `rsrt` | 0.3.6 | MIT OR Apache-2.0 | github.com/cesbo/rsrt | Sep 2026 | dependency (in use since batch 2). Pure Rust, tokio, HaiCrypt AES; listener and caller can both send. Its authors document interop with libsrt 1.4.4; on 17 Sep 2026 we ran its 668 tests incl. 101 interop tests against the libsrt 1.5.6 installed here, and a manual libsrt→rsrt run (700 pkts, 0 lost). Fallbacks: `shiguredo_srt` (sans-I/O, Apache-2.0), `srt-tokio` (stale 2024) |
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
| RTSP **server** | github.com/bluenviron/gortsplib | superseded 18 Sep 2026: Rust server pieces exist after all (see M9 below) |
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
| Design system | Material Design 3 spec; tokens from `@material/material-color-utilities` 0.4.0 (Apache-2.0); components in React + Tailwind to the M3 spec. `@material/web` rejected: maintenance mode | Apache-2.0 / MIT |
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
| 2 | **RTSP server** | glue `rtsp-types` + `sdp-types` + webrtc-rs `rtp`, modelled on `rtsp-runtime` (see M9 below) | medium |
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

## Backlog tiers: existing Rust code per feature

Surveyed 17 Sep 2026 by three agents, then every star count, date and
license re-checked by hand with `gh api` and crates.io. Tiers match
`PLAN.md` → Backlog. Pure Rust unless stated.

### Tier C (later)

| Feature | Repo / crate | Version | License | Stars | Last push | Decision |
|---|---|---|---|---|---|---|
| HDR10 static metadata (HEVC SEI) | quietvoid/hevc_parser (`hevc_parser`) | 0.6.12 | MIT | 32 | Aug 2026 | depend |
| Dolby Vision RPU | quietvoid/dovi_tool (`dolby_vision` lib crate) | 3.4.0 | MIT | 1,017 | Sep 2026 | depend |
| HDR10+ | quietvoid/hdr10plus_tool (`hdr10plus` lib crate) | 2.1.5 | MIT | 458 | Apr 2026 | depend |
| Playlist / linear channels | ffplayout/ffplayout | — | **GPL-3.0** | 589 | Sep 2026 | reference only, never link |
| VOD MP4 demux | kixelated/mp4-atom | 0.15.0 | MIT OR Apache-2.0 | 30 | Sep 2026 | depend (already chosen) |
| Audio demux/decode | pdeljanov/Symphonia | 0.6.1 | **MPL-2.0** | — | Aug 2026 | depend unmodified only if needed; file-level copyleft |
| NDI | grafton-ndi and other bindings | 1.0.0 | Apache-2.0 bindings over **proprietary Vizrt SDK** | 34 | Jun 2026 | skip; SDK cannot ship with an OSS binary |
| NDI alternative | cool-japan/oximedia (`oximedia-videoip`) | 0.2.1 | **no license file** | 257 | Sep 2026 | skip until licensed and proven; single author, created Feb 2026 |
| PSSH boxes (DRM ids) | emarsden/pssh-box-rs (`pssh-box`) | 0.2.5 | MIT | 16 | Jul 2026 | depend |
| CENC/CBCS packaging | vbasky/sheathe | 0.6.1 | **no license file** | 16 | Sep 2026 | reference only; write our `senc`/`tenc` writer on mp4-atom |
| AES-128 HLS segments | `aes` + `cbc` crates | — | MIT/Apache | — | — | write ourselves (small) |
| Widevine / FairPlay packager | none legitimate in Rust | — | — | — | — | skip; both need vendor licensing |
| DASH MPD generation | emarsden/dash-mpd-rs (`dash-mpd`) | 0.20.4 | MIT | 111 | Sep 2026 | depend (has a write path) |
| WebVTT/TTML → HLS | fishloa/rust-broadcast, subtitle-rs | 0.1.x / 2.7.1 | Apache-2.0 | 1 / 3 | 2026 | write ourselves; thin glue over WebVTT segments |
| Audio-only HLS (AAC/Opus) | compose mp4-atom + our packager | — | — | — | — | no extra crate needed |
| RTMP push out (YouTube/Twitch) | KallDrexx/rust-media-libs (`rml_rtmp`) client side; xiu's RTMP client as reference | 0.8.0 | MIT | 242 | Apr 2023 (stale) | depend for primitives, write the push wrapper; check scuffle-rtmp client mode first |
| HLS pull input | sile/hls_m3u8 + aschey/stream-download-rs | 0.7.0 / 0.24.4 | Apache-2.0 | 66 / 114 | 2026 | depend; write the live-edge polling loop |

### Tier A (table stakes)

| Feature | Repo / crate | Version | License | Stars | Last push | Decision |
|---|---|---|---|---|---|---|
| Enhanced RTMP (HEVC/AV1/Opus FourCC) | ScuffleCloud/scuffle `scuffle-flv` | 0.2.2 | MIT OR Apache-2.0 | 418 | Apr 2026 | depend: source has `VideoFourCc`/`AudioFourCc`, Hevc, Av1, Opus (grep-verified) |
| Enhanced RTMP, alternative | moq-dev/moq `moq-rtmp` (15K lines, "RTMP / enhanced-RTMP ingest gateway") | 0.2.11 | MIT OR Apache-2.0 | 1,526 | Sep 2026 | depend if we go MoQ-first (see tier B) |
| Enhanced RTMP, small | torresjeff/rtmp-rs | 0.6.0 | MIT | 8 | Sep 2026 | reference |
| WHIP ingest | algesten/str0m (engine) + 8xFF/atm0s-media-server (MIT, 330★, WHIP/WHEP reference) | 0.23.1 | MIT | 624 / 330 | Sep 2026 | depend on str0m; write the WHIP HTTP layer; live777 (311★) is **MPL-2.0**, read only |
| WebRTC engine, alternative | webrtc-rs/webrtc | 0.20.x | Apache-2.0 | 5,145 | Sep 2026 | fallback to str0m |
| SRTLA bonding | irlserver/srtla_send (Rust, MIT, 34★, Sep 2026); yannismate/srtla-rs (5★, Jan 2025); BELABOX/srtla is **AGPL** C | — | MIT | — | — | reference; write ourselves on top of `rsrt` |
| SRT stream-id routing | none | — | — | — | — | write ourselves (small; `rsrt` exposes stream id) |
| Failover / backup source | none in any Rust server | — | — | — | — | write ourselves in `caudal-core` |
| Hot config reload | `config` 0.15 + `arc-swap` 1.9 + `notify` 9.0 | — | MIT/Apache | — | 2026 | depend |
| JSON logs / OTel | `tracing-subscriber` 0.3, `tracing-opentelemetry` 0.33, `opentelemetry-otlp` 0.32 | — | MIT/Apache | — | 2026 | depend |
| Metrics / shutdown | `metrics-exporter-prometheus` 0.18, `axum-prometheus` 0.10, `tokio-graceful-shutdown` 0.20 | — | MIT/Apache | — | 2026 | depend |
| SCTE-35 | rafaelcaricio/scte35 (MIT, 9★, Jun 2026) or dholroyd/scte35-reader (Apache-2.0, 8★, Apr 2026) | 0.2.0 / 0.16.0 | MIT / Apache-2.0 | 9 / 8 | 2026 | depend on one; both small |
| ID3 | `id3` | 1.17.1 | MIT/Apache | — | Jul 2026 | depend |
| fMP4 `emsg` | fishloa/rust-broadcast `mp4-emsg` (1★, Aug 2026) | 0.4.0 | Apache-2.0 | 1 | Aug 2026 | reference; write on mp4-atom |
| CEA-608/708 captions | none on crates.io | — | — | — | — | write ourselves |
| WebVTT | `webvtt` 0.2 (2023), `subtp` 0.2 (2024), both stale | — | unverified | — | — | write ourselves |
| Object storage | apache/arrow-rs `object_store` (S3/GCS/Azure/local) | 0.14.2 | Apache-2.0 | 3,613 | Sep 2026 | depend; write the segment uploader |
| Object storage, wider | apache/opendal | 0.59.2 | Apache-2.0 | 5,382 | Sep 2026 | alternative |
| MP4 clip by time range | kixelated/mp4-atom; video-commander/mp4box (`mp4box` 0.14, MIT, 6★, Sep 2026, "non-destructive editing") | — | MIT/Apache | — | 2026 | depend on mp4-atom; write the cut |
| H.264 / HEVC decoder for thumbnails | **none in pure Rust** (`openh264` is C bindings, 127★; BSD-2-Clause per its Cargo.toml) | — | — | — | — | external ffmpeg |
| AV1 decoder | memorysafety/rav1d (pure-Rust port of dav1d) | — | BSD-2-Clause | 643 | Aug 2026 | depend for AV1 thumbnails |
| arm64 static builds | rust-cross/cargo-zigbuild + `aarch64-unknown-linux-musl` | 0.23.4 | MIT/Apache | 2,657 | Sep 2026 | depend; `cross` is stale (2023) |
| Helm chart | teknoir/mediamtx-helm (0★, no license) | — | — | 0 | Jul 2026 | write ourselves |

### Tier B (differentiators)

| Feature | Repo / crate | Version | License | Stars | Last push | Decision |
|---|---|---|---|---|---|---|
| WASM plugins, engine | bytecodealliance/wasmtime + wit-bindgen (component model) | 48.0 | Apache-2.0 | 18,641 | Sep 2026 | depend: frame filters need typed zero-copy buffers |
| WASM plugins, easy PDK | extism/extism (plugins in Go/JS/Python too) | 1.4 | BSD-3-Clause | 5,762 | Sep 2026 | depend for triggers/auth decisions |
| Latency stamps (SEI / `emsg`) | dholroyd/h264-reader (SEI parsing, Apache-2.0, 97★); Eyevinn/mp4ff (Go, MIT) as wire-format reference; Glass2GlassHQ/glass2glass is **MPL-2.0**, 4★ | 0.9.0 | Apache-2.0 | 97 | Sep 2026 | write ourselves (small writer/reader); hls.js `LatencyController` as spec |
| Network impairment (`caudal lab`) | moqtap/moqtap `quinn-netem` (UDP loss/delay/reorder, MIT, 1★, Sep 2026); oguzbilgener/noxious (Toxiproxy-compatible, 55★, stale 2023) | 0.1.3 | MIT | 1 | Sep 2026 | reference; write our UDP proxy. `tokio-rs/turmoil` (MIT, 1,260★) for deterministic tests: depend |
| **MoQ-first fan-out** | moq-dev/moq: `moq-mux` (43K lines: fMP4/CMAF, MKV, TS, FLV ↔ hang broadcast), `moq-hls` (LL-HLS gateway, 4.9K lines), `moq-rtmp`, `moq-srt`, `moq-relay` (cluster + JWT), `moq-rtc`, `moq-transcode` | 0.9.16 / 0.4.16 / 0.2.11 | MIT OR Apache-2.0 | 1,526 | Sep 2026 (daily) | **depend. This is the biggest reuse win of the survey: the "everything is a MoQ broadcast" server already exists as a crate family.** Read `rs/moq-mux/src` and `doc/bin/relay/auth.md` before M1 |
| Rate limiting | boinkor-net/governor + `tower_governor` | 0.10.4 / 0.8.0 | MIT | 938 / 351 | Aug 2026 / Aug 2025 | depend |
| API keys / tenant quotas | small unverified crates only | — | — | — | — | write ourselves (thin axum middleware) |
| GitOps config | toml-rs/toml `toml_edit` (comment-preserving), GitoxideLabs/gitoxide `gix` (pure-Rust git), GREsau/schemars | — | Apache-2.0 / MIT | 1,074 / 11,960 / 1,411 | Sep 2026 | depend on all three |
| `caudal doctor` | webrtc-rs `stun` 0.17, `rsntp` 4.1 (MIT/Apache), `x509-parser` 0.18 (MIT/Apache), `rustls` | — | MIT/Apache | — | 2026 | depend; codec probe reuses our demuxers |
| Conformance in CI | Apple `mediastreamvalidator` via `xcrun` on macOS runners; rust-fuzz/cargo-fuzz (1,896★), proptest (2,237★); cesbo/rsrt `tests/support` as the ffmpeg/libsrt harness pattern | — | Apache-2.0 | — | 2026 | depend on fuzz/proptest; write the harness |


## Per milestone: what exists (deep pass, 18 Sep 2026)

Two Sonnet agents read source, not just READMEs; every crate, license and
repo re-checked by hand with crates.io and `gh api`.

### M6 · MoQ / WebTransport
- **Publish our frames from inside the process** (verified in `rs/hang/examples/video.rs`, `rs/moq-mux/src/container/producer.rs`): `moq_net::Origin::random().produce()` → `origin.create_broadcast("", Route::new().with_announce(true))` → `broadcast.create_track(name, info)` → `moq_mux::container::Producer::new(track, Container::Legacy)` → `producer.write(Frame { timestamp, payload, keyframe, duration })`. `payload` is length-prefixed NALs, i.e. our AVCC frames as they are; the avcC goes in the catalog's `VideoConfig.description` (`hang::catalog::H264 { inline: false, .. }`).
- **Relay embeds in our process** (`moq-relay` is lib + bin). Self-signed certs work with SHA-256 fingerprint pinning (`rs/moq-native/src/tls.rs`), the native twin of the browser's `serverCertificateHashes`.
- **Browser player:** `@moq/watch` 0.5.4 and `@moq/hang` 0.4.3 on npm, MIT OR Apache-2.0. Bundle size unverified.
- Other Rust MoQ: `moqtail/moqtail` (Apache-2.0, ★103, draft-18, reference), `cloudflare/moq-rs` (reference), `shiguredo/moqt-rs` (too early). moq-net negotiates IETF drafts 14–21; cross-implementation interop not tested by us.
- **Plan:** depend on `moq-net` + `hang` + `moq-mux` + `moq-native`; embed `moq-relay`. Our ring stays the source; a MoQ output subscribes and writes into a `Producer`.

### M4 (rest) · SRT out
- `rsrt` sends as listener (viewers pull) and as caller (push to a remote), with TSBPD pacing.
- **TS muxer exists:** `moq-mux`'s `container/ts/export.rs` (1,914 lines) is a complete live muxer: PAT/PMT every 500 ms and on keyframes, PCR every 25 ms, AVCC→Annex B with SPS/PPS re-injection, AAC→ADTS, built on `mpeg2ts` (which writes as well as reads).
- **Plan:** `moq-mux` TS export + `rsrt` send. Nothing to write but glue.

### M8 · Recording, DVR, VOD
- `mp4-atom` (already a dependency) has the full sample tables (`stts`, `stsz`, `stsc`, `stco`/`co64`, `stss`, `ctts`), so a progressive MP4 writer is a moov builder on top of it, next to `crates/caudal-hls/src/fmp4.rs`.
- `muxide` 0.2.5 (MIT OR Apache-2.0): zero-dependency MP4 muxer for recording (H.264/H.265/AV1/AAC/Opus). Trial it before writing our own.
- `shiguredo_mp4` 2026.5.0 (Apache-2.0, Shiguredo): mature alternative.
- Crash-safe recording: `webm-iterable` 0.7.1 (MIT) can write EBML; `matroska` (tuffy) reads only.
- `object_store::put_multipart` (0.14.2, Apache-2.0) for segment upload to S3/R2/GCS; `m3u8-rs` already writes `#EXT-X-PLAYLIST-TYPE:VOD` (`MediaPlaylistType::Vod`).
- **Plan:** record fMP4 segments (we already make them) + a VOD playlist; final MP4 via `muxide` or an `mp4-atom` moov builder; upload with `object_store`.

### M9 · RTSP
- **Pull cameras:** `retina` 0.4.20 (MIT/Apache, ★370), see `examples/client`.
- **Serve RTSP, pure Rust, corrected:** `rtsp-types` 0.1.3 + `sdp-types` 0.2.0 (MIT, sdroege) for the protocol; webrtc-rs `rtp` 0.17 for H.264 FU-A packetizing; `rtsp-runtime` 0.6.0 (MIT OR Apache-2.0, sans-I/O client+server state machine, single author) as the model or a fork; `shiguredo_rtsp` (Apache-2.0, pre-1.0) and `msf-rtsp` 0.3.1 (MIT) to trial. xiu has a working pure-Rust RTSP server (`protocol/rtsp/src/session/server_session.rs`, crate `xrtsp`, MIT, stale on crates.io since Aug 2024) as a reference.
- Rejected: `oddity-ai/oddity-rtsp` (links ffmpeg via `video-rs`), `gstreamer-rtsp-server` (LGPL C), `webrtc-sdp` (MPL-2.0), `rtp-rs` (no license).
- **Plan:** no gortsplib port. Glue `rtsp-types` + `sdp-types` + `rtp`, modelled on `rtsp-runtime`.

### M10 · Clustering
- `moq-relay`'s `cluster.rs`: full mesh with hop-cost routing, static peers, an HTTP/file peer list, or gossip; JWT per peer.
- Pull-on-demand pattern: xiu `protocol/rtmp/src/relay/pull_client.rs` pulls from a remote origin when a local viewer subscribes to a stream with no local publisher. Our LL-HLS edges do the same over MoQ.
- Topology reference: `atm0s-media-server` (MIT): console / gateway / connector / media node roles, GeoIP routing.
- Membership: keep `chitchat` + `hashring`. Rejected: `al8n/memberlist`, `al8n/serf` (MPL-2.0), `rendezvous` (EUPL-1.2).
- **Plan:** embedded `moq-relay` between nodes; edges pull on first viewer.

### M11 · Transcoding / ABR
- Pure Rust: `rav1e` 0.8.1 (AV1 encode), `rav1d` 1.1.0 (AV1 decode), BSD-2. No pure-Rust H.264/HEVC/AAC encoder worth using.
- **External ffmpeg:** `ffmpeg-sidecar` 2.5.2 (MIT, ★539) drives ffmpeg as a process, which keeps GPL out of our binary.
- **macOS hardware encode:** `objc2-video-toolbox` 0.3.2 (Zlib OR Apache-2.0 OR MIT).
- `moq-dev/moq` `moq-transcode` / `moq-video`: per-rung ABR over hang broadcasts with NVENC / VideoToolbox / Media Foundation / openh264. Only reusable as-is if Caudal's internal model becomes `hang`; otherwise reference `rs/moq-transcode/src/{ladder,rung,pipeline}.rs`.
- Avoid: `fdk-aac` (restrictive libfdk license), `opus`/`audiopus` (C bindings).
- **Plan:** `ffmpeg-sidecar` for H.264/AAC ladders, VideoToolbox on Macs, `rav1e` for AV1.
