# STATUS — Caudal

**Last updated:** 21 Sep 2026 (v0.1 code complete; 120-min soak running; tag pending)

## What it is
Open-source rewrite of MistServer in Rust. Full plan and evidence in `PLAN.md`; reuse inventory in `REUSE.md`.

## RESUME HERE (21 Sep 2026) — v0.1 code complete, tag pending
**Saul's rules** (memory `caudal-permisos`, `saul-pregunta-no-es-pedido`): merge when CI is green; up to 5 agents but ONE heavy local job at a time (lock `/private/tmp/caudal-build.lock`, `CARGO_BUILD_JOBS=2`, `nice`); benchmarks and soak on GitHub (`bench.yml`, `soak.yml`), never on the laptop; a question is not a request.

**main = `86baee1`.** All four v0.1 code items merged:
- #20 soak test (`soak.yml`, `bench/soak.py`): 30-min run PASSED (RSS slope -15 MB/h, growth -0.7 %, fd 115 and threads 11 flat, 3 streams / 63 viewers, 0 errors, reload + clip ok) — run 35464718699. **120-min run dispatched on main: 35606887553.**
- #21 memory per live stream: 90 -> 55 MB (10 s in) and 145 -> 65 MB (steady) vs MediaMTX 84/93; heap profile found RTMP slice retention, hang's 30 s MoQ cache, LL-HLS double-held segments; `[buffer] window_secs` default 50 -> 15 (user-visible).
- #22 batched UDP sends (GSO): x300 Linux 300/300 kept up, 1,803 Mbps, CPU 179 % vs main's 30/300 and 263 %; x100 60 % vs MediaMTX 104 %.
- #23 release pipeline (`release.yml`): tag `v*.*.*` builds static musl binaries + SHA256SUMS + multi-arch GHCR image, publishes the GitHub Release from `docs/release-notes/v0.1.0.md`. Dry run green (35464788814); a manual dispatch never publishes.
- #19 docs: `docs/QUICKSTART.md`, `docs/OBS.md` (Saul's glass-to-glass procedure).

**Before tagging v0.1.0:** (1) 120-min soak green; (2) update `docs/release-notes/v0.1.0.md` with the final RSS / batched-send / soak numbers (the "Idle-publisher RSS" line still says MediaMTX is lighter — that is now fixed); (3) main CI green. Then `git tag -a v0.1.0 && git push origin v0.1.0` (publishes binaries + `ghcr.io/saul-punybz/caudal:0.1.0`).
**Fixed 21 Sep (PR #24):** the Firefox-only "LL-HLS stall" was not a stall — Firefox honours `autoplay` only at HAVE_ENOUGH_DATA (Chromium/WebKit start at HAVE_FUTURE_DATA), so the `<video>` stayed paused 2.8–6.4 s while hls.js held the playhead and the live edge ran away, past hls.js's catch-up band. `play.html` now calls `play()` on canplay and seeks to `liveSyncPosition` if stranded; the spec waits for progress while unpaused; the blanket firefox CI retry is gone. Firefox ingest-to-glass 3.70 s → 1.22–1.42 s, 5 consecutive green CI runs. Details in `tests/browser/NOTES.md`.
**Still open from that work:** (a) `moq.spec.ts` on firefox is flaky (canvas never reaches 1280, twice, then passed; untouched page, a main control rerun passed); (b) one run had firefox steady state pinned at 2.61–2.67 s, inside hls.js's band but not converging (`forwardBufferLength` gate); (c) the guard's forward seek inflates `currentTimeDelta`, so a decoded-frame counter (TEST-AUDIT gap 12) would make that assertion airtight.
**Set aside:** MistServer bench (`bench/mistserver`), MCP server (future).
**After v0.1:** kTLS + sendfile, io_uring/pacing, OMT protocol, Raspberry Pi image, MoQ on Safari, DASH, SDKs, watermarking, CEA-608, GPU transcoding, TEST-AUDIT phase 2/3.
**Pages:** status https://claude.ai/artifact/NGCn3AzWdLEKLGQkvuT56A · brief https://claude.ai/artifact/UYhTQHp2aJrS754w2W4J7w
**Rule from Saul:** verify with tools outside Claude; say what is not verified.

## Finding, 19 Sep 2026: scuffle-rtmp froze timestamps
`scuffle-rtmp` 0.2.3 (latest) returned the previous header unchanged for every Type 3 chunk, so a Type 3 chunk that starts a new message kept the previous timestamp instead of adding the delta (RTMP spec 5.3.1.2.4; FFmpeg `rtmppkt.c`). Any encoder that sends constant-rate frames as Type 3 got frozen timestamps **on Caudal's RTMP ingest**. Found by `caudal-restream`'s loopback test. Patched copy in `vendor/scuffle-rtmp` (`[patch.crates-io]`), regression test fails upstream (`[10, 50, 50, 50]`) and passes patched. Also fixed: the SRT tests probed TCP for free ports while SRT binds UDP (failed every full-workspace run); `caudal-restream`'s push loop now only reads inside `select!` (a cancelled write could corrupt the chunk stream).

## Where we are (18 Sep 2026, evening)
| Area | State |
|---|---|
| Ingest | RTMP/E-RTMP, SRT, WHIP, RTSP pull (retina), 24/7 channel from files (M13) |
| Output | LL-HLS (Apple-validated, multi-rendition, rendition reports), WHEP, MoQ, SRT push/listen, RTSP server (TCP interleaved, UDP unicast, RTSPS), SCTE-35 cues (DATERANGE, TS), multistreaming RTMP/RTMPS push (`[[restream]]`, `/api/v1/restreams`) |
| Processing | Transcoding ladders (ffmpeg default, rusty_h264 in-process), recording + VOD + clips |
| Platform | TOML config with hot reload, API, metrics, TLS/HTTP2/ACME, tokens + webhooks, admin login (password, OIDC, API tokens), health alerts, M3 UI (Overview, Stream, Publish, Channels, Restreams, Recordings, Login) |
| Running | Benchmark vs MediaMTX (next) |
| Next | Batch 9: SCTE-35 (`docs/research/SCTE35.md`), admin login, health alerts. Roadmap in `PLAN.md` |
| OMT | `vmx-codec` ported in pure Rust, byte-identical to libvmx both ways; public repo `Saul-Punybz/open-media-transport` |

## Finding, 17 Sep (evening)
SRT and RIST **do exist in pure Rust**: `rsrt` (cesbo, verified against libsrt 1.5.6: 668 tests + interop) and `rist-core` (wavey-ai, Simple + Main profiles, interop against librist). Details in `REUSE.md`. No need to port gosrt or libRIST.

## Finding, 18 Sep (early)
Three Sonnet agents surveyed backlog tiers A, B and C; every number re-verified by hand. Full tables in `REUSE.md`. Biggest win: **moq-dev/moq already has `moq-mux`, `moq-hls`, `moq-rtmp` (enhanced RTMP), `moq-srt` and `moq-relay`**. M1 starts with an evaluation of building on `hang` + `moq-mux`.

## Plan verification, 18 Sep 2026
Checked the moq-dev/moq crates by reading source, not descriptions:
- `moq-mux` (597 tests) and `moq-rtmp` (193 tests) are solid.
- `moq-hls` emits **plain HLS only**: no `EXT-X-PART`, `_HLS_msn` or preload hints. The LL-HLS packager is still ours.
- `moq-srt` is thin (18 tests); SRT uses `rsrt` directly.
- `hang` shipped 17 releases in 90 days. Any use gets pinned to an exact version and wrapped behind a `caudal-core` trait; its types never reach our API.
- Neither `hang` nor `moq-relay` keeps history: no DVR, no failover. `caudal-core` stays as that layer.

**Decision:** batch 1 builds directly on `caudal-core` + `scuffle-rtmp`/`scuffle-flv` + `mp4-atom`. No `hang` yet. Batch 2 runs a half-day spike on `moq-mux` for TS/MKV/FLV import reuse, with a real stream in hand.

## Goal, one sentence
OBS publishes RTMP to one `caudal` binary and a browser plays it as LL-HLS with under 3 s glass-to-glass, on a laptop, with nothing else installed.

## Minimal list (in the order the signal travels)
| # | Piece | State | Size |
|---|---|---|---|
| 1 | Live buffer, media model (`caudal-core`) | DONE | — |
| 2 | RTMP ingest → `Publisher` (`caudal-rtmp`) | DONE | — |
| 3 | LL-HLS packager + `/play/{name}` page (`caudal-hls`) | DONE (browser playback unconfirmed, see below) | — |
| 4 | Server shell: TOML config, `/api/v1/streams`, `/metrics`, `/healthz`, graceful shutdown (`caudal`) | DONE | — |
| 5 | Wiring in `main.rs`, CI, `FROM scratch` Dockerfile | DONE (CI not yet run on GitHub; Docker not built: daemon off) | — |
| 6 | Watch in a browser, measure latency | PARTIAL | batch 2 |

## Not built in batch 1
SRT, RIST, WebRTC, MoQ, auth/JWT, ACME, React UI, recording, clustering, transcoding, MistServer import, `hang` integration. Each has its milestone in `PLAN.md`.

## Definition of done, per piece
- **2 RTMP:** `ffmpeg -re -i x.mp4 -c copy -f flv rtmp://localhost:1935/live/test` and OBS both publish; `GET /api/v1/streams` shows `test` with H.264 + AAC tracks and rising `frames_in`; a wrong app name is rejected; disconnect ends the stream within 5 s. Enhanced RTMP (HEVC) publishes too.
- **3 LL-HLS:** `/hls/test/index.m3u8` passes `mediastreamvalidator` with parts of 200 ms and blocking reload; hls.js and Safari play it; `/play/test` is one HTML page with hls.js that shows measured latency; a viewer that joins mid-stream starts within 2 s.
- **4 Shell:** `caudal --config caudal.toml` starts with only `[server] http_bind`; `caudal check caudal.toml` rejects a bad key with the line number; SIGTERM drains viewers; `/metrics` has `caudal_viewers{stream}` and `caudal_bytes_in_total{stream}`.
- **5 Wiring:** `cargo build --release` yields one binary under 20 MB; `docker run -p 1935:1935 -p 8080:8080 caudal` works; CI runs tests + clippy + `cargo deny` licenses.
- **6 Close:** the goal sentence, done by a person, on video or with a screenshot and the latency number.

## Batch 1: who does what
Names below are fixed. Agents consume them; they do not invent new ones.

| Agent | Model | Crate / files | Cost (API-price equivalent) |
|---|---|---|---|
| A · shell | Sonnet | `crates/caudal/src/{config,api,metrics,shutdown}.rs` | ~$3 |
| B · RTMP | Sonnet | `crates/caudal-rtmp/**` | ~$3 |
| C · LL-HLS | **Opus** (the error here costs the most and shows up last) | `crates/caudal-hls/**`, `crates/caudal-hls/static/play.html` | ~$5 |
| D · CI + Docker | Haiku | `.github/workflows/ci.yml`, `Dockerfile`, `deny.toml`, `justfile` | ~$0.5 |
| Orchestrator | Opus | shared files only: `Cargo.toml`, `crates/caudal-core/**`, `crates/caudal/src/main.rs`, `STATUS.md` | ~$5/h |

**Shared names, fixed now.**
- Crates: `caudal`, `caudal-core`, `caudal-rtmp`, `caudal-hls`.
- Config (TOML): `[server] http_bind = "0.0.0.0:8080"`, `[rtmp] bind = "0.0.0.0:1935"`, `[rtmp] app = "live"`, `[hls] part_ms = 200`, `[hls] segment_ms = 2000`, `[buffer] window_secs = 50`, `[buffer] max_mb = 256`.
- HTTP: `GET /api/v1/streams`, `GET /api/v1/streams/{name}`, `GET /hls/{name}/index.m3u8`, `GET /hls/{name}/init.mp4`, `GET /hls/{name}/{segment}.m4s`, `GET /play/{name}`, `GET /metrics`, `GET /healthz`, `GET /readyz`.
- Seam: every protocol crate takes `Arc<caudal_core::Registry>` and nothing else from the server.
- Each crate exposes one entry point, already stubbed in `main` (commit 00f35c0): `caudal_rtmp::serve(RtmpConfig { bind, app, buffer }, registry) -> io::Result<()>`, `caudal_hls::router(registry, HlsConfig { part_ms, segment_ms }) -> axum::Router`.
- Core additions for batch 1: `Codec::as_str()`, `TrackKind::as_str()`, `Registry::subscribe_publishes()`.
- API JSON shape for a stream is fixed in agent A's brief: `{name, tracks:[{id,kind,codec,timescale,width,height,fps,sample_rate,channels,lang}], stats:{frames_in,bytes_in,frames_buffered,bytes_buffered,buffered_ms,viewers}}`.

**Budget, said aloud:** batch 1 ≈ $12 in agents + ≈ $8 orchestrator over ~1.5 h ≈ **$20 API-price equivalent**, plus a third unassigned for the fix round ⇒ **≈ $30, two batches** (build, then fix + close). On the $100 membership these are token equivalences, not charges. Out of scope: everything in "Not built in batch 1".

**Rules for every agent:** fresh context, the seven-item brief, report in ≤15 lines as `file:line → what`, run `cargo test -p <crate>` and `cargo clippy -- -D warnings` before answering, never touch shared files, ask for a name instead of inventing one. Worktree per agent, branch `agent/<piece>`; the orchestrator merges branch by branch with tests between each.

## Tests: three layers
1. **Component:** each crate's own tests (`caudal-core` has 13). Required in every agent brief.
2. **Contract:** the fixed names checked in code: agent A's config parser rejects unknown keys; routes exist.
3. **End-to-end:** `crates/caudal/tests/e2e.rs` + `tests/support/mod.rs`, written 18 Sep before batch 1. Drives the real binary with ffmpeg over RTMP, asserts the API, the LL-HLS playlist tags, blocking reload, fMP4 parts, the `/play` page, and runs Apple's `mediastreamvalidator` when installed. Run: `CAUDAL_E2E=1 cargo test -p caudal --test e2e -- --test-threads=1`. **Red today (5/5 fail: binary has no `/healthz`)**; batch 1 is done only when it is green and step 6 (a person with OBS) confirms it.

## Batch 1 result (closed 18 Sep 2026)
Merged A (shell), B (RTMP, on scuffle-rtmp 0.2.3 + scuffle-flv), C (LL-HLS on mp4-atom 0.15), D (CI/Docker/deny).

**Verified**
- `cargo test --workspace`: 47 passed, 0 failed. `cargo clippy -D warnings`: clean. `cargo deny check`: advisories, bans, licenses, sources ok.
- E2E harness: **5/5 green**, stable over 4 consecutive runs (`CAUDAL_E2E=1 cargo test -p caudal --test e2e -- --test-threads=1`).
- Release binary: **2.2 MB**. RSS with one live 720p stream: **13.6 MB**.
- Live run of the release binary with ffmpeg publishing 1280x720 H.264 + AAC over RTMP: API lists the stream and tracks; ffprobe reads the LL-HLS playlist; a 10 s live decode with ffmpeg has zero errors.
- Server-side latency (ingest → newest part published): median 0.11 s, max 0.20 s over 10 samples. Plus PART-HOLD-BACK 0.6 s.

**Not verified (said aloud)**
- **Playback in a real browser.** Chrome under automation keeps the tab `hidden` and throttles timers, so hls.js never attaches (same for agent C). Needs a person to open `http://127.0.0.1:8080/play/demo` once, or a headless-Chrome check in CI (batch 2).
- **Glass-to-glass latency.** Estimated ~1–1.5 s from the numbers above; not measured end to end.
- **Apple mediastreamvalidator: now VERIFIED locally** (18 Sep 2026, v1.26.143, installed at `/usr/local/bin`). Run with `CAUDAL_E2E=1 CAUDAL_REQUIRE_HLS_VALIDATOR=1 just e2e`. Result on `master.m3u8`: multivariant + LIVE media playlist, 0 parse errors, 0 critical errors, avc1 + aac; the -50102 hold-back SHOULD is fixed. Two MUSTs remain, allow-listed with reasons in `crates/caudal/tests/support/mod.rs` (`KNOWN_MUST`): -50120 HTTP/2 (removed by M7 TLS), -50125 rendition report (unsatisfiable with one rendition: the validator also rejects a self-report, -50099; removed by ABR in M11). **GitHub's macOS runner does not have the tool**; CI shows a NOT VERIFIED warning banner and requires it automatically when present.
- The first validator run exposed a false green: the old detector looked for the word `ERROR`, which Apple's tool never prints (it exits 0 and labels issues CRITICAL / MUST). The canary test (`validator_rejects_a_broken_playlist`) caught it. The parser now reads the real sections and is covered by `validator_output_parser`.
- **CI on GitHub: green** (18 Sep 2026): test, deny, validate-hls (macOS), static builds for x86_64 and aarch64 musl. The first run caught a race in the e2e blocking-reload check on slow runners; the check now asks one segment ahead and verifies the answer. **Docker image** not built (Docker daemon off).

**Known issues from the agents' notes**
- Wrong RTMP app / busy stream name: no stream is created, but the connection is not closed; scuffle-rtmp's error type has no custom variant (`crates/caudal-rtmp/NOTES.md`).
- No integration test for two publishers racing for one name.
- `fps` is null in the API for RTMP sources (only filled from onMetaData when present).

## UI design (18 Sep 2026)
Material Design 3 with the brand palette (Orange `#F54F1B`, Space Cadet `#1E223D`, Gargoyle Gas `#E6D5B7`). Rules in `ui/DESIGN.md`; generated tokens in `ui/theme/` (all text pairs WCAG AA). Mockup canvas: https://claude.ai/artifact/4Lr5fgqvk5Dj58HhfXzMjq (Overview dark/light, Stream detail, Palette); source copies in `ui/mockups/`. The React app itself is not built yet.

## Next
Batch 2: (1) headless-browser playback check in CI (Playwright or chromedriver against `/play`), plus glass-to-glass measured by decoding the burned-in clock; (2) close RTMP connections on rejection; (3) `moq-mux` spike; (4) M4 SRT via `rsrt` (done in batch 2).

## Batch 8 (18 Sep 2026): M13 24/7 channels, multistreaming, UI screens
- **Y, 24/7 channel (merged):** `crates/caudal-channel`. `[[channel]] name, items, loop, shuffle`; `GET /api/v1/channels`, `POST /api/v1/channels/{name}/skip`. MP4/MOV + TS, real-time pacing (100 ms lead), timestamps stitched across files and loops, mismatched files skipped with the reason in the API. 10 tests. **Real binary:** two 6 s MP4s from a directory; LL-HLS decoded by ffmpeg, RTSP played by ffprobe (H.264 640 + AAC), skip moved to item 2 with 204. Added `TsDemux::flush` (the last frame of every TS file was lost).
- Gaps: files must share codec parameters (normalization via caudal-transcode later), no fragmented MP4, no MKV, no schedules.
- **Shutdown:** once, after a clean shutdown log, the process stayed alive (tokio waits forever for blocking tasks on runtime drop). Not reproducible in 4 retries; main now calls `shutdown_timeout(5 s)`.
- Running: Z multistreaming (`caudal-restream`), UI agent (Channels, Restreams, Recordings screens; Recordings had an API without a screen since batch 6).

## Batch 7 result (18 Sep 2026)
- **U, RTSP (merged):** retina pull (credentials split from URL, reconnect 1→30 s, permissive initial timestamp), RTSP server over TCP interleaved (H.264 FU-A, H.265 FU, AAC RFC 3640), auth 401/403, 404. 5 tests incl. ffprobe decode and pull-reconnect. **UDP transport answers 461**: ffmpeg falls back to TCP, some cameras/VLC defaults may not — backlog.
- **V, transcoding (merged):** see PLAN M11; rusty_h264 2–3x x264 CPU, ~4.7 dB lower PSNR.
- **W, multi-rendition HLS (merged):** Safari joins low-latency 8/8 with rendition reports.
- **X, OMT VMX codec:** 2,463 lines, `forbid(unsafe_code)`, no deps; encoder bytes identical to libvmx and decoders cross-identical (17 conformance tests, NEON reference). 1080p UYVY one thread: 110 fps encode (C 318), 458 fps decode (C 1171). Found an upstream libvmx thread-pool shutdown hang (report upstream). Pushed to private `Saul-Punybz/open-media-transport`.
- Verified after merges: clippy 0 warnings, deny ok, e2e 15/15 (65.9 s), channel 10/10, rtsp 5/5.

## Batch 7 (launched 18 Sep 2026): M9 RTSP, M11 transcoding + multi-rendition HLS, M12 OMT
**Goal:** cameras in and RTSP out; an ABR ladder whose renditions group into one multivariant playlist (clears Apple -50125 and tests the Safari bimodal hypothesis); the first pure-Rust VMX codec for OMT.

Refactor first (orchestrator): the MPEG-TS mux/demux moved from `caudal-srt` into a shared crate `caudal-ts` (`ts::TsDemux`, `demux::Demuxer`, `mux::TsMux`), so the transcoder can pipe TS to and from ffmpeg. SRT tests unchanged and green.

| Agent | Model | Owns | Done when |
|---|---|---|---|
| U · RTSP | Sonnet | `crates/caudal-rtsp/**` | pulls via `retina` (reconnect forever) publish cameras; RTSP server (`rtsp-types` + `sdp-types` + webrtc-rs `rtp`) serves any live stream over TCP interleaved with `Access::Play`; ffmpeg/ffprobe `rtsp://` tests |
| V · transcode | **Opus** | `crates/caudal-transcode/**` | ladder renditions published as `<name>+<label>`; `Ffmpeg` engine pipes TS stdin/stdout via `caudal-ts` (process group, killed on drop, restarted on crash); `RustyH264` engine trialed and benchmarked vs ffmpeg/x264 on this Mac; AAC out for every rendition (so WHIP/Opus sources become Safari-audible) |
| W · multi-rendition HLS | Sonnet | `crates/caudal-hls/**` | `master.m3u8` of `<name>` lists `<name>` and live `<name>+*` (BANDWIDTH/RESOLUTION/CODECS), each media playlist carries `EXT-X-RENDITION-REPORT` for its siblings, tokens propagate; Apple validator shows no -50125 |
| X · OMT | **Opus** | new repo `~/Downloads/_Projects/open-media-transport/` | crate `vmx-codec`: pure-Rust port of libvmx (MIT) decoder + encoder, scalar first, bit-exact against the C reference in tests (C built only in dev/test), MIT notices kept, MIT OR Apache-2.0 |
| Orchestrator | Opus | refactor, config, main, e2e | `[rtsp]`, `[[rtsp.pull]]`, `[transcode]` config (done), merges, e2e |

**Fixed names:** `caudal_rtsp::{serve, RtspConfig { bind, pulls, buffer }, RtspPull { stream, url }}`; `caudal_transcode::{start, TranscodeConfig { ladders, engine, ffmpeg, buffer }, Ladder { streams, renditions }, Rendition { label, height, video_kbps, audio_kbps }, Engine::{Ffmpeg, RustyH264}}`; rendition stream name `<name>+<label>`; config `[rtsp] bind`, `[[rtsp.pull]] stream, url`, `[transcode] engine, ffmpeg`, `[[transcode.ladder]] streams`, `[[transcode.ladder.rendition]] label, height, video_kbps, audio_kbps`.

**Coordination (heat):** every agent builds with `CARGO_BUILD_JOBS=2`; heavy tests one at a time; processes spawned by tests live in their own process group and are killed on drop; no `--release`.

**Budget:** U ≈ $3.5, V ≈ $6, W ≈ $2.5, X ≈ $7, orchestrator ≈ $8, a third held back ⇒ **≈ $40 API-price equivalent** (token equivalence on the $100 membership, not a charge). **Out of scope:** clustering (M10), OMT protocol/discovery (next OMT batch), GitHub repo for OMT (orchestrator creates it private after review).

## Batch 6 result (18 Sep 2026)
All three agents stalled once on a service watchdog (600 s without progress); R's WIP was saved and resumed, S restarted from scratch. Merged R (recording: CMAF segments + VOD playlist, crash-safe temp+rename, restart recovery, retention, ordered object_store upload with retries; clips as progressive MP4 streamed from segment byte ranges with edit lists; StartAt::Oldest), S (SRT out: `play/<name>` pull and `[[srt.push]]` with reconnect; MPEG-TS mux on `mpeg2ts` — moq-mux's exporter needs a hang broadcast; H.264 + AAC verified, H.265 untested, Opus not muxed; fixed a trailing-PES bug), T (WebKit over HTTPS in the browser suite; Safari bimodal finding).
**Verified:** workspace **158/158**, e2e **15/15** (new: record → VOD → clip), cargo-deny ok, clippy clean, no leftover processes.
**Not verified:** object-storage upload against real S3/GCS (file:// only); H.265 over SRT out; Opus over SRT out (not muxed).

## Batch 6 (launched 18 Sep 2026): M8 recording + VOD, SRT out, Safari over HTTPS in CI
**Goal:** record any stream to disk (optionally S3/R2), replay it as VOD, cut clips; send streams out over SRT; the browser suite measures Safari over HTTPS.

Refactor first (orchestrator): the fMP4 writer and Opus helpers moved from `caudal-hls` into a new shared crate `caudal-cmaf` (`fmp4::{init_segment, fragment, Mp4Track, Run, Sample}`, `opus::{parse_opus_head, frame_duration_samples}`), so the recorder reuses the tested writer. HLS tests unchanged and green.

| Agent | Model | Owns | Done when |
|---|---|---|---|
| R · recording | **Opus** | `crates/caudal-record/**` | `start(registry, RecordConfig)` records matching streams as CMAF segments cut on keyframes + a growing VOD `index.m3u8` (ENDLIST at end), `meta.json`; retention sweep; optional `object_store` upload; routes per lib.rs doc; clip endpoint writes a progressive MP4 (moov with sample tables) for a time range cut on keyframes; tests incl. ffprobe of VOD and clip |
| S · SRT out | Sonnet | `crates/caudal-srt/**` | `play/<name>` (and `m=request`) pulls a stream as MPEG-TS over SRT with `Access::Play`; `pushes` push streams to remote listeners with reconnect; TS mux from AVCC/AAC/Opus (moq-mux TS export if usable standalone, else `mpeg2ts` writer); tests with `srt-live-transmit` as receiver + ffprobe |
| T · Safari HTTPS | Haiku | `tests/browser/**` | harness can start Caudal with `[tls]` (self-signed cert via openssl, `ignoreHTTPSErrors`); WebKit runs play + steady tests over HTTPS; the known-gap branch becomes a < 3 s assertion when served over HTTP/2 |
| Orchestrator | Opus | refactor, config, main, e2e | `[record]` and `[srt] push` config (done), e2e: record → VOD playable, clip download |

**Fixed names:** `caudal_record::{start, RecordConfig { dir, streams, segment_secs, retention_hours, upload_url }, RecordService::router}`; routes `GET /api/v1/recordings`, `GET|DELETE /api/v1/recordings/{stream}/{id}`, `GET /vod/{stream}/{id}/index.m3u8`, `GET /vod/{stream}/{id}/{file}`, `POST /api/v1/clips {stream, id, from_ms, to_ms}`; id = `YYYYMMDDTHHMMSSZ`; `caudal_srt::{SrtConfig { .., pushes }, SrtPush { stream, url }}`; config `[record] enabled, dir, streams, segment_secs, retention_hours, upload_url`, `[[srt.push]] stream, url`.

**Budget:** R ≈ $5, S ≈ $3, T ≈ $0.5, orchestrator ≈ $6, a third held back ⇒ **≈ $20**. **Machine rule:** one heavy run at a time, debug binaries for tests, no release builds during tests; any test that spawns `srt-live-transmit` uses a process-group Drop guard.

## Safari over HTTP/2 (measured 18 Sep 2026, after Saul ran `mkcert -install`)
Release binary with `[tls]` and a mkcert certificate for localhost; ffmpeg RTMP 720p publish.
- `curl --http2` without `-k`: **HTTP/2, 200** (the mkcert CA is trusted).
- Apple `mediastreamvalidator` over HTTPS: multivariant + LIVE, 0 parse errors, avc1 + aac, **-50120 gone**. Only -50125 remains (needs a second rendition, M11).
- **Safari's native player (Playwright WebKit on macOS, 30 s): steady ingest-to-glass 0.52 s** (was 5.4–6 s over HTTP/1.1). Confirms the hypothesis: Apple's player only stays in low-latency mode over HTTP/2.
- Browser suite now runs WebKit over HTTPS (batch 6 T, self-signed openssl cert, `ignoreHTTPSErrors` on the webkit project). **Finding: Safari's native player over HTTP/2 is bimodal.** In 6 runs it joined either in low-latency mode (0.41–0.85 s) or in normal mode (4.07–4.36 s) and stayed there. Tests guard < 5 s and annotate the normal-mode joins. **Cause confirmed (18 Sep 2026):** with two renditions and rendition reports (batch 7 W), Safari joined in low-latency mode **8/8** (0.75–2.01 s), versus 3/6 without them. Rendition reports are what keep Apple's player in low-latency mode; single-rendition streams still lack them by spec (-50099 forbids self-reports).

## Batch 5 result (18 Sep 2026): M6 MoQ
Merged P (MoQ output: moq-native server + one origin, no relay needed; each stream a `hang` broadcast via moq-mux, H.264/H.265 + AAC/Opus; self-signed P-256 cert, 13-day validity, rotated every 6 days live; `/moq/fingerprint`; viewer counts from moq-net stats; `?jwt=` auth per path) and Q (UI: 3-way LL-HLS / WebRTC / MoQ toggle with `@moq/watch` 0.5.4 rendering to canvas, lazy chunk ≈ 123 KB gzip; Outputs rows; Playwright test).

**Verified:** caudal-moq 13/13 (native client over WebTransport with the fingerprint pinned: catalog avc1 + mp4a, groups open on IDR, 1 viewer counted, broadcast ends with the source); UI 39/39; **MoQ plays in Chromium through the UI: 97 frames decoded in 3 s, AAC audio bytes received**; cargo-deny ok; clippy clean.

**MoQ in Firefox: verified** (18 Sep 2026): 90 frames decoded in 3 s, AAC audio bytes arriving, same as Chromium. **Not verified:** AAC actually audible (bytes arrive; WebCodecs support varies by browser); MoQ ingest (out of scope).

## Batch 5 (launched 18 Sep 2026): M6 MoQ output
**Goal:** every live stream is also a Media over QUIC broadcast; a browser plays it over WebTransport with the `@moq/watch` player, no mkcert needed.

| Agent | Model | Owns | Done when |
|---|---|---|---|
| P · MoQ | **Opus** | `crates/caudal-moq/**` | `start(registry, MoqConfig)` binds QUIC, embeds moq-relay (or moq-native server + origin), publishes each registry stream as a `hang` broadcast named `<stream>` via `moq-mux` `Producer` (avcC → catalog `description`, Opus/AAC audio); self-signed ECDSA cert < 14 days, rotated; `GET /moq/fingerprint`; Rust test subscribes with a moq-native client and receives a keyframe |
| Q · player | Sonnet | `ui/app/**`, `crates/caudal-ui/dist/**`, `tests/browser/tests/moq.spec.ts` | "MoQ" in the player's protocol toggle using `@moq/watch` (fingerprint from `/moq/fingerprint`); Playwright test plays a stream in Chromium over WebTransport |
| Orchestrator | Opus | wiring (done), e2e, merge | `/moq/fingerprint` e2e, merge, cool-down between heavy runs |

**Fixed names:** `caudal_moq::{start, MoqConfig { bind, cert }, MoqCert::{Files{cert,key}, SelfSigned{hosts}}, MoqService::router}`; route `GET /moq/fingerprint` → `{"url": "https://host:port", "fingerprint": "<sha-256 hex>" | null}`; broadcast name = stream name; config `[moq] enabled = true`, `bind = "0.0.0.0:4443"`, `cert`, `key`, `hosts`.

**Pins:** moq-net 0.2.22, hang 0.20.13, moq-mux 0.9.16, moq-native 0.19.19, moq-relay 0.14.18 (all 17 Sep 2026), `@moq/watch` 0.5.4 — exact `=` versions.

**Out of scope:** MoQ ingest (publishing into Caudal over MoQ), clustering (M10). **Budget:** P ≈ $5, Q ≈ $3.5, orchestrator ≈ $6, a third held back ⇒ **≈ $20**. **Machine rule:** one heavy run at a time; no release builds while tests run.

## Batch 4 result (18 Sep 2026): M5 WebRTC
Merged M (str0m engine: WHIP ingest, WHEP playback, one UDP socket, ICE-lite, PLI, token gate, CORS; pure-Rust crypto backend), N (Opus in LL-HLS: `Opus`/`dOps`, TOC durations, `CODECS="opus"`), O (UI: LL-HLS/WebRTC toggle, WHEP player with buffer readout, `/publish` webcam page over WHIP, play tokens in the UI).

**Verified:** caudal-webrtc 14/14 (ffmpeg WHIP publish → H.264 1280x720 + Opus, DELETE, 409/400/406, gate 401/403/201, a str0m WHEP client receives IDR with SPS/PPS + Opus); caudal-hls 19/19 (Opus); UI 29/29; e2e `whip_publish_plays_as_ll_hls` **passes** against the real binary (ffmpeg 9 WHIP muxer); **WHEP plays in Chromium through the UI** (`tests/browser/tests/whep.spec.ts`: 2.82 s advanced in 3 s, ≈ 9 ms jitter buffer, AAC-source note shown).

**Not verified yet:** WHEP in Firefox (not run: the laptop was hot after a release build); the `/publish` webcam page against a real camera; Safari native HLS with Opus audio (Apple's validator flags `-50010 Unrecognized codec: opus`, since Apple's HLS spec has no Opus; hls.js browsers play it).

**Known limits:** no TURN / server-reflexive candidates / trickle ICE (PATCH → 405); AAC sources play over WHEP video-only (needs M11 audio transcoding); B-frame sources flagged, sent with pts timestamps; A/V start offset from first arrival, not RTCP SR.

## Batch 4 (launched 18 Sep 2026): M5 WebRTC
**Goal:** publish from a browser or ffmpeg over WHIP and it plays everywhere (LL-HLS and WHEP); play any stream over WHEP with sub-second latency.

| Agent | Model | Owns | Done when |
|---|---|---|---|
| M · WebRTC | **Opus** | `crates/caudal-webrtc/**` | str0m, one UDP socket; `POST /whip/{name}` → publishes H.264 + Opus into the registry (AVCC, keyframes, SPS→width/height); `POST /whep/{name}` → sends H.264 (and Opus audio when the source has it) with RTCP PLI → next keyframe; Bearer/`?token=` via `Registry::authorize`; B-frame sources flagged; tests with ffmpeg's WHIP muxer |
| N · Opus in HLS | Sonnet | `crates/caudal-hls/**` | Opus tracks in init.mp4 (`Opus`/`dOps`) and fragments; `CODECS="opus"`; plays in Chromium/Firefox via hls.js |
| O · UI | Sonnet | `ui/app/**`, `crates/caudal-ui/dist/**` | WebRTC row in Outputs becomes live: WHEP player toggle next to HLS with its latency; play tokens passed through; a `/publish` page that publishes the webcam over WHIP |
| Orchestrator | Opus | core, `crates/caudal/**`, e2e | wiring (done in scaffold), e2e `whip_publish_plays_as_ll_hls` (written, red), WHEP browser test after merge |

**Fixed names:** `caudal_webrtc::router(registry, WebRtcConfig { udp_bind, public_ips, buffer })`; routes `POST /whip/{name}`, `DELETE /whip/{name}/{session}`, `POST /whep/{name}`, `DELETE /whep/{name}/{session}`; config `[webrtc] udp_bind = "0.0.0.0:8189"`, `public_ips = []`; codec name `"opus"` (`Codec::Opus`), `TrackInfo::init` for Opus = the OpusHead bytes (RFC 7845 §5.1), timescale 48000.

**Known limits, said aloud:** RTMP/SRT sources usually carry AAC, which WebRTC can't play without transcoding (M11): WHEP of such streams is video-only. WebRTC browsers don't decode B-frames; sources with them are flagged.

**Budget:** M ≈ $5 (Opus), N ≈ $2.5, O ≈ $3.5 → ≈ $11 + orchestrator ≈ $8, a third held back ⇒ **≈ $28 API-price equivalent**. **Machine rule** as before: heavy tests one at a time, no leftover ffmpeg.

## Batch 3 result (18 Sep 2026)
Merged K (TLS: rustls on ring, h1+h2 via ALPN, cert hot reload, ACME), J (JWT/JWKS auth, Standard Webhooks), L (steady-state latency test). Orchestrator wired it: core `Gate` + `Registry::authorize` + `subscribe_ends`; tokens read from RTMP stream keys, SRT stream ids and HLS `?token=` / Bearer; every playlist URI carries the viewer's token forward; `[tls]`, `[auth]`, `[hooks]` config with validation; HTTPS runs next to HTTP with one shutdown signal.

**Verified:** e2e **13/13** (new: publish refused without / with a wrong token, accepted with the right one; play 401 / 403 / 200 with the token on init and parts; HTTPS negotiates HTTP/2). Workspace 100/100, clippy clean, cargo-deny ok (RUSTSEC-2023-0071 ignored with reason: we never use RSA private keys; rustls-pemfile replaced).

**Found and fixed on the way:** the `/play` page's `liveSyncDurationCount: 1` pinned hls.js at a 2 s target instead of 0.6 s. Steady-state now: Chromium 1.66 s, Firefox 1.10 s.

**Not verified:** ACME against a real CA (needs a public domain). **Safari over HTTP/2** (needs a certificate this Mac trusts: Saul runs `mkcert -install`; then Caudal with a mkcert cert, measure WebKit native and rerun Apple's validator over HTTPS to clear -50120). The React UI does not pass play tokens yet; the admin API has no auth yet.

## Batch 3 (launched 18 Sep 2026): M7 TLS, HTTP/2, auth
**Goal:** Caudal can run on a public server: HTTPS with HTTP/2 (Apple's last MUST, -50120, and the likely cause of Safari's ~6 s), tokens to publish and play, webhooks on stream start/end.

| Agent | Model | Owns | Done when |
|---|---|---|---|
| K · TLS | Sonnet | `crates/caudal-tls/**` | `serve(TlsConfig, Router, shutdown)` serves h1 + h2 via ALPN; cert files hot-reloaded; ACME via rustls-acme (TLS-ALPN-01, cache dir, staging flag); tests with an rcgen self-signed cert: `curl --http2 -k` negotiates h2, reload picks up a new cert without restart |
| J · auth | Sonnet | `crates/caudal-auth/**` | `Authorizer::check(action, stream, token)`: HS256 secret or JWKS URL (cached, refreshed, kid lookup), claims `sub` (name or `prefix*`), `act`, `exp`; `Hooks::emit` delivers Standard Webhooks-signed JSON with retries, never blocking; unit tests incl. a local JWKS server |
| L · latency | Haiku | `tests/browser/**` | a second test that samples ingest-to-glass every second for 30 s per browser and reports min/median/max at steady state (after the first 10 s) |
| Orchestrator | Opus | root `Cargo.toml`, `crates/caudal/**`, ingest/output crates | config `[tls]`, `[auth]`, `[hooks]`; token extraction in RTMP (`?token=` on the stream key), SRT (stream id), HLS (`?token=` or `Authorization: Bearer`); e2e tests |

**Fixed names (scaffold commit):** `caudal_tls::{serve, TlsConfig { bind, source }, CertSource::{Files{cert,key}, Acme{domains,email,cache_dir,staging}}}`; `caudal_auth::{Authorizer::new(AuthConfig{keys,publish,play}), Authorizer::check, Action::{Publish,Play}, KeySource::{Secret, Jwks{url,refresh}}, AuthError::{Missing,Invalid,Forbidden}, Hooks::new(Option<HooksConfig{urls,secret}>), Hooks::emit(HookEvent::{StreamStarted,StreamEnded})}`.

**Budget:** ≈ $10 agents + ≈ $8 orchestrator, a third held back ⇒ **≈ $25 API-price equivalent**. **Out of scope:** WebRTC, MoQ, recording, clustering. **Needs Saul:** `mkcert -install` (adds a local CA to the keychain) before Safari over HTTP/2 can be measured locally.

**Machine rule:** heavy tests one at a time; after every run, no leftover `ffmpeg`/`srt-live-transmit` of ours.

## Batch 2 progress (18 Sep 2026)
- **Merged:** F (rejected RTMP publishers are disconnected), E (Playwright: Chromium plays at 2.4 s ingest-to-glass; WebKit plays but at ~6 s, tracked as a known gap until HTTP/2), I (React + Material 3 UI embedded in the binary; release binary 3.0 MB).
- **Fixed by orchestrator:** viewer counts (the HLS packager counted as a viewer; HLS players were not counted at all), UI icon size and headline weight.
- **SRT (G), finished by the orchestrator:** G's tests leaked `srt-live-transmit` processes, which busy-loop at 100% CPU once their input ends; five of them overheated the laptop, so G was stopped. Fixed with a process-group Drop guard (`crates/caudal-srt/tests/srt.rs`, `Pipeline`), plus a 3 s `data_idle_timeout` so a killed caller ends its stream in time. **Rule for every test that spawns srt-live-transmit: own process group, killed in Drop, never wait for it to exit by itself.**
- **Browsers (18 Sep 2026, after adding hls.js live catch-up `maxLiveSyncPlaybackRate: 1.5`):** local Mac: Chromium 2.16 s, Firefox 2.33 s, Safari native 5.61 s (known gap). GitHub Linux: Chromium 2.18 s, Firefox 2.28 s, WebKit-with-hls.js 1.81 s. All numbers are ingest-to-glass shortly after startup; steady-state not yet measured.
- **Closed with:** e2e **10/10**, workspace tests 63/63, clippy clean, cargo-deny ok, zero leaked processes. Browser: Chromium 2.4 s ingest-to-glass; WebKit ~6 s (known gap, HTTP/2).

## Batch 2 (launched 18 Sep 2026)
**Goal:** a real browser plays Caudal, proven by a test; SRT is a second way in; the web UI shows live streams from the real API; rejected RTMP publishers get disconnected.

| Agent | Model | Owns | Done when |
|---|---|---|---|
| E · browser | Sonnet | `tests/browser/**`, `.github/workflows/browser.yml` | Playwright (Chromium + WebKit) opens `/play/e2e` against the real binary with ffmpeg publishing; video `readyState ≥ 3`, `currentTime` advances, hls.js latency and ingest-to-glass (`Date.now() - playingDate`) reported and under 3 s |
| F · RTMP reject | Sonnet | `crates/caudal-rtmp/**` | wrong app and busy name close the TCP connection within 1 s; race test for two publishers on one name |
| G · SRT | Sonnet | `crates/caudal-srt/**` | `srt-live-transmit` → `srt://…?streamid=publish/test` appears in `/api/v1/streams` with H.264 + AAC and plays as LL-HLS; AES passphrase works; wrong passphrase rejected. TS demux: moq-mux's TS container if usable without a hang broadcast, else `mpeg2ts` |
| I · UI | Sonnet | `ui/app/**`, `crates/caudal-ui/dist/**` | React + Vite + TS + Tailwind with `ui/theme/tokens.css`; Overview and Stream detail per the mockups, fed by `/api/v1/streams` (polling 1 s); `/play` embedded; `npm run build` writes into `crates/caudal-ui/dist/` |
| Orchestrator | Opus | root `Cargo.toml`, `crates/caudal/**` (config, main, e2e), `crates/caudal-core/**`, `ci.yml`, `justfile`, STATUS | merges, adds SRT to e2e, wires `just ui` |

**Fixed names (in code at commit below):** `caudal_srt::serve(SrtConfig { bind, latency_ms, passphrase, buffer }, registry)`; config `[srt] bind = "0.0.0.0:9000"`, `latency_ms = 120`, `passphrase` (optional); stream id `publish/<name>` or `#!::r=<name>,m=publish`; `caudal_ui::router()` merged last as the fallback, serving `crates/caudal-ui/dist/` at `/`.

**Budget, said aloud:** E ≈ $2.5, F ≈ $1.5, G ≈ $4, I ≈ $4 → ≈ $12 in agents + ≈ $8 orchestrator ≈ $20, plus a third held back ⇒ **≈ $30 API-price equivalent** (token equivalence on the $100 membership, not a charge). **Out of scope:** WebRTC, MoQ, auth, TLS, recording, clustering, `moq-mux` beyond the TS demux question.

**Batch 1 launched 18 Sep 2026** from commit 00f35c0: A (Sonnet), B (Sonnet), C (Opus), D (Haiku), each in its own worktree. Orchestrator merges branch by branch, running the e2e harness between merges. Step 6 is automated (ffmpeg publishes, Chrome opens `/play`, screenshot with the latency number); OBS is optional.

## Decisions
- Name **Caudal** (free on crates.io as of 17 Sep 2026).
- License MIT OR Apache-2.0. MistServer is Unlicense (public domain), so nothing restricts the port.
- One async process (tokio), not one process per connection like MistServer.
- 100% Rust, nothing in C or Go linked. ffmpeg only as an external process for transcoding.
- HDS, Flash and Smooth Streaming are not ported.

## How to run
```
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## Reference source
MistServer clone: `git clone --depth 1 https://github.com/DDVTECH/mistserver`.
