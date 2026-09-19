# Prompt for Google Gemini (Deep Research): blind spots

Written 19 Sep 2026. Paste everything below the line. The answer goes to
`docs/research/BLINDSPOTS.md` only after every claim is checked by hand (in two
earlier reports about a third of the claims were wrong or unverifiable).

---

You are a senior systems engineer and reviewer who knows live video infrastructure (ingest, packaging, delivery, players) and production Rust (async runtimes, networking, unsafe, performance, supply chain). Your job is to find what the Caudal team is **not** looking at: areas to improve, develop or optimize that are missing from the lists below. Do not repeat what the team already has or has planned unless you have evidence that the plan is wrong.

## Rules (read first)

- Cite a source for every non-obvious claim: URL plus the date you saw it. For a crate, give the exact crates.io name, latest version, license, last release date and repository. For a paper, benchmark or issue, link the exact page.
- If you cannot verify something, write **UNVERIFIED** next to it. Never invent crate names, versions, benchmark numbers, CVE ids, RFC numbers or GitHub issues. A missing answer is better than an invented one.
- Label each item **[verified]** (you opened the source), **[inferred]** (reasoned from verified facts) or **[speculative]**.
- Prefer primary sources: RFCs and specs, crate source code and docs, maintainers' posts, GitHub issues of MediaMTX / SRS / OvenMediaEngine / MistServer / Ant Media / LiveKit / hls.js / Shaka / ffmpeg.
- Today is September 2026. Prefer 2025–2026 sources and say when something is older.

## What Caudal is

Caudal is an open-source (MIT OR Apache-2.0) live media server written in **pure Rust**. It is a from-scratch rewrite of MistServer (C++, ~187K lines). It ships as one static binary (musl, `FROM scratch` Docker image) with an embedded React UI, about 30K lines of Rust in 20 crates, on tokio. Repository: https://github.com/Saul-Punybz/caudal (public).

**Architecture.** One async process. `caudal-core` holds a per-stream ring buffer; frames are shared by reference (`bytes::Bytes`), never copied per viewer; a slow viewer skips to the next keyframe instead of holding memory; each track keeps its native timescale (no millisecond rounding). Every protocol is a crate that publishes into or subscribes from that core. Config is one TOML file held as an immutable snapshot (`arc-swap`) with hot reload (SIGHUP or an API call): only the sections that changed are restarted.

**Ingest.** RTMP and Enhanced RTMP (H.264, HEVC, AV1; `scuffle-rtmp` 0.2.3, patched in-tree for a chunk timestamp bug), SRT with AES (`rsrt` 0.3.6), WebRTC WHIP (`str0m` 0.23), RTSP camera pull (`retina` 0.4.13), 24/7 linear channels from MP4/TS files, SCTE-35 cues (TS stream_type 0x86, RTMP onCuePoint, HTTP API; `scte35-splice` 2.1.0).

**Output.** LL-HLS / CMAF built on `mp4-atom` 0.15 (passes Apple's `mediastreamvalidator`, multi-rendition with rendition reports, `EXT-X-DATERANGE` for SCTE-35), WebRTC WHEP, Media over QUIC over WebTransport (`moq-net`/`hang`/`moq-mux`, quinn), SRT out (pull and push), RTSP server (TCP interleaved, UDP unicast, RTSPS), RTMP/RTMPS multistreaming push (`rml_rtmp` 0.8), MPEG-TS with a SCTE-35 PID.

**Processing.** Adaptive-bitrate ladders through an external ffmpeg process by default, or in-process with `rusty_h264` 0.16 (pure-Rust H.264); recording to disk and S3-compatible storage, VOD, MP4 clips.

**Platform.** rustls with the ring provider, HTTP/2, automatic ACME; JWT/JWKS tokens for publish and play; Standard Webhooks signed events; admin login (argon2id passwords, OIDC code+PKCE via `openidconnect` 4.0.1, API tokens, server-side sessions with CSRF tokens); stream health alerts (missing keyframes, bitrate floor, lost publisher) as webhooks; Prometheus metrics; Material Design 3 UI.

**Measured so far.** Release binary 11.7 MB. RSS about 14 MB with one live 720p stream. Server-side ingest to newest LL-HLS part: median 0.11 s. A side-by-side benchmark against MediaMTX v1.21 is running now (CPU, RSS, fan-out at 1/100/1,000 viewers over LL-HLS, RTSP and WHEP, glass-to-glass latency).

**Testing today.** Unit, contract and end-to-end tests (about 300), the end-to-end suite drives the real binary with ffmpeg and runs Apple's validator; GitHub Actions CI; `cargo deny`, `cargo audit`, clippy with `-D warnings`. Being added: fuzzing, mutation testing, interop with VLC / GStreamer / OBS / browsers, packet captures.

**Already planned (do not propose these unless the plan is wrong):** origin-edge clustering on `moq-relay`, backup-source failover, live captions (local speech-to-text to WebVTT and CEA-608), geo-blocking and IP lists, recording schedules, DASH, MoQ ingest and Safari 26.4+ MoQ, Helm chart, Raspberry Pi image, `caudal doctor`, MistServer config import, WASM plugins (wasmtime), per-viewer QoE beacons, network impairment lab, multi-tenancy with quotas, forensic watermarking, player SDKs, HDR metadata passthrough, CENC/ClearKey, and a pure-Rust Open Media Transport (an open NDI alternative, with a byte-exact port of the VMX codec).

## What to research

Work through each area. For each finding give: **what** it is, **why it matters for Caudal specifically** (tie it to the architecture above), **evidence**, **how to do it in pure Rust** (named crates with versions, or "no crate exists; closest reference is X in language Y, license Z"), **effort** (S/M/L), **risk if ignored**.

1. **Pure-Rust gaps in the media stack.** Where does "pure Rust" cost us today: codecs (H.264/HEVC/AV1/Opus/AAC encode and decode; `rusty_h264`, `rav1e`, `rav1d`, others), audio resampling and loudness, image/overlay work, SRT/RIST maturity, QUIC stacks (quinn vs s2n-quic vs quiche/tokio-quiche for HTTP/3 and MoQ), DTLS/SRTP. Which gaps can realistically be closed in Rust, which would need FFI, and which crates are close but unmaintained.
2. **Performance we are leaving on the table.** Kernel and I/O techniques for high fan-out delivery from a Rust server: `io_uring` (tokio-uring, monoio, glommio; how they fit with tokio), `sendfile`/`splice`, kTLS with rustls, UDP GSO/GRO and `sendmmsg` for WebRTC/SRT/RTP, `SO_REUSEPORT` sharding, thread-per-core vs work-stealing for media, allocator choice (mimalloc, jemalloc) and fragmentation in long-running servers, SIMD for packetization/checksums/crypto, HTTP/2 and HTTP/3 settings for LL-HLS blocking playlist reloads at 10K+ viewers, caching of segment bytes. Include measured numbers from real projects with links, not guesses.
3. **Correctness risks specific to live media in async Rust.** Cancellation safety in `tokio::select!` (we already found one bug of this kind), backpressure and bounded queues, clock domains and drift (wall clock vs media clock, 33-bit PTS wrap, A/V sync, B-frame reorder), timestamp discontinuities on publisher reconnect, leap seconds and NTP steps in `PROGRAM-DATE-TIME`, long-run (weeks) resource leaks. Name tools that prove absence of bugs: `loom`, `shuttle`, `kani`, `miri`, `cargo-careful`, property testing, deterministic simulation (e.g. `turmoil`, madsim), with what each can and cannot cover here.
4. **Interop and conformance we are not testing.** Official or community conformance suites and test vectors: HLS (beyond mediastreamvalidator: hls.js, Shaka, ExoPlayer/Media3, AVPlayer, Roku, smart TVs, set-top boxes), CMAF/DASH-IF conformance, WebRTC (browser matrix, WHIP/WHEP interop lists), SRT (Haivision interop, libsrt versions), RTMP (OBS, vMix, Wirecast, hardware encoders), RTSP (ONVIF profiles, camera quirks), SCTE-35 (SSAI vendors' expectations for DATERANGE vs CUE-OUT). Which quirks break real devices, with issue links.
5. **Security and supply chain beyond the basics.** Threat model for a public media server: amplification on UDP (RTP/RTCP, SRT, STUN), resource exhaustion per connection, slowloris on LL-HLS blocking requests, SSRF through pull/push URLs and webhooks, token replay, admin session attacks, header/URL parsing, zip-bomb-like media. Supply chain: dependency count and trust, `cargo-vet`, `cargo-crev`, SBOM, reproducible builds, signed releases (Sigstore), `unsafe` in our dependency tree (`cargo-geiger`) and which dependencies carry the most.
6. **Operations people will need and nobody asks for until it breaks.** Upgrades without dropping viewers (socket handoff, graceful drain), config migrations, observability (OpenTelemetry traces through the media path, per-stream structured logs), capacity planning, cost per viewer-hour, disaster recovery for recordings, time sync requirements, IPv6 and dual-stack, NAT and CGNAT for WebRTC (TURN), running behind CDNs (cache keys and headers for LL-HLS, origin shielding).
7. **Things the market wants that are absent from both lists above.** Only items with real demand evidence (issue reactions, forum threads, vendor headline features, RFPs), especially for small operators, churches, schools, municipalities, local TV/radio and Spanish-speaking markets.
8. **Where the plan itself looks wrong.** Any planned item above that is the wrong priority, the wrong approach, or already solved by an existing crate or standard.

## Output format

1. A table of the **top 15 findings** ranked by (impact on Caudal) × (confidence), with columns: #, area, finding, why it matters, evidence (links), pure-Rust path (crates + versions), effort, confidence label.
2. One section per research area with the full findings.
3. A list of **crates mentioned**, each with: name, version, license, last release, repository, maintained? (commits in last 6 months), and whether it is pure Rust or wraps C.
4. A list of everything marked UNVERIFIED, so it can be checked by hand.
