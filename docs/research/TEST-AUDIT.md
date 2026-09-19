# Test audit: does Caudal's testing prove it works for real users?

**Date:** 19 Sep 2026 · **Commit audited:** `4b8a7c5` · **Method:** read-only. Code, tests,
workflows and GitHub Actions logs were read (`gh run view --log`); nothing was compiled or
run, because a CPU benchmark was running on the machine. Anything that needs a run is listed as
**to run**.

## Short answer

No. Caudal has a large and mostly honest test suite (≈300 Rust tests, 15 e2e tests against the
real binary, 12 Playwright cases in three real browsers, one Apple validator run). But the
evidence that it works end to end **for real users** is much thinner than the counts suggest:

1. **The Linux CI test job has not passed once since GitHub Actions came back.** Every `CI` run
   in `gh run list` from 18 Sep 19:51 to 19 Sep 00:29 is `failure`. In the latest run with logs
   (`35409072226`), `cargo test --workspace` stopped at
   `crates/caudal-health/tests/integration.rs:69` ("webhook(s) not delivered in time", after 92 s).
   `cargo test` has no `--no-fail-fast`, so the crates after `caudal-health` in build order
   (hls, moq, record, restream, rtmp, rtsp, scte35, srt, tls, transcode, ts, webrtc) **did not
   run on Linux**, and the `CAUDAL_E2E=1` step was skipped. The next run (`35409545989`) failed
   the same step too; its log was not out yet when this was written. The "298/298" and "e2e
   15/15" figures in STATUS are local macOS runs.
2. **Some tests CI runs cannot fail, and some it never runs at all.** CI never installs `srt-live-transmit`, so every SRT
   integration test prints `SKIP` and counts as passed (the macOS log shows
   `SKIP: srt-live-transmit not on PATH` next to `test srt_publish_plays_as_ll_hls ... ok`).
   GitHub's macOS runner does not have `mediastreamvalidator`, so the validator step and its
   canary are both skipped. `crates/caudal/tests/reload.rs` needs `CAUDAL_E2E=1`, but CI only
   sets that flag for `--test e2e`, so hot reload has never run in CI.
3. **Most oracles are Caudal's own code.** Where a test has a real client on the other end, it
   is usually `ffmpeg`/`ffprobe`. VLC, GStreamer, OBS, TSDuck, threefive, a real IdP, a real CA,
   a real S3 bucket, a real camera, an iPhone and an Android player are never used. Several
   interop tests are circular: str0m talks to str0m, moq-native to moq-native, and the RTSP pull
   is tested against Caudal's own RTSP server.
4. **Release builds use `panic = "abort"` (`Cargo.toml:53`), but every test uses a debug build.**
   In tests, tokio catches a panic in a spawned task and the test may still pass. In production,
   the same panic kills the whole server, and every stream with it. Nothing is fuzzed, which
   breaks PLAN.md's own rule: "Every parser gets a fuzz target before it touches the network".
   Together these are the biggest unmeasured risk.
5. **Nothing runs longer than about 60 seconds.** No soak test, no fd or memory growth check, no
   slow-viewer test at the HTTP level, and no test where a publisher reconnects. The known RTMP
   49.7-day timestamp wrap has no test.

The rest of this document is the detail and the fix order.

---

## (a) Capability × layer matrix

Legend: ✅ covered and meaningful · ⚠️ weak (see note) · ❌ missing · **CI?** = does GitHub CI
actually execute it today (given facts 1–2 above). **Indep.** = the verdict comes from a third
party (tool, player, spec vector), not from Caudal's own code.

Layers: **U** unit · **C** contract (config/route shape) · **I** in-process integration
(crate + real client) · **E** e2e against the real `caudal` binary · **B** real browser ·
**V** external validator · **D** real device/encoder · **CI**.

| Capability | U | I | E | B | V / D | Indep. oracle | CI? | Key evidence (file:line) |
|---|---|---|---|---|---|---|---|---|
| RTMP ingest (H.264/AAC) | ⚠️ cue AMF only | ✅ ffmpeg publish | ✅ | ✅ (feeds all browser tests) | ❌ OBS, vMix, hardware encoders | ffmpeg (encoder side only) | I: ❌ (after health failure); E: macOS ✅ | `caudal-rtmp/tests/rtmp.rs:126`, `e2e.rs:44` |
| RTMP enhanced (HEVC) | ❌ | ✅ ffmpeg libx265 | ❌ | ❌ | ❌ OBS 30+ E-RTMP | ffmpeg | ❌ | `rtmp.rs:368` |
| RTMP reject (wrong app, busy name, bad name) | — | ✅ | ⚠️ no positive control | — | — | self | macOS e2e only | `rtmp.rs:206,246,329`, `e2e.rs:81` |
| RTMP timestamp wrap (u32 ms, 49.7 d) | ❌ | ❌ | ❌ | — | — | — | — | bug at `caudal-rtmp/src/demux.rs:52` (BLINDSPOTS #4) |
| RTMP chunk parser (vendored scuffle) | ⚠️ one regression test | ✅ via restream loopback | — | — | ❌ fuzz | self (restream ↔ rtmp, both ours) | ❌ | `vendor/scuffle-rtmp`, `caudal-restream/tests/loopback.rs:102` |
| SRT ingest (listener, AES) | ✅ streamid | ✅ libsrt `srt-live-transmit` | ⚠️ skipped w/o tool | — | ❌ OBS SRT, hardware (Haivision, Kiloview) | libsrt ✅ | **❌ always SKIP** | `srt.rs:229,354`, `e2e.rs:254` |
| SRT out (play/, push) | ✅ URL parse | ✅ libsrt receiver + ffprobe | ❌ | — | ❌ | libsrt + ffprobe | **❌ SKIP** | `srt.rs:403,502` |
| WHIP ingest | ✅ codec | ✅ ffmpeg WHIP | ✅ | ❌ `/publish` webcam page | ❌ OBS 30 WHIP, real camera | ffmpeg ≥ 8 | Linux: SKIP (apt ffmpeg 6.1); macOS e2e ✅ | `webrtc.rs:162`, `e2e.rs:443` |
| WHEP playback | ✅ | ⚠️ str0m client vs str0m server | ❌ | ✅ Chromium, **❌ Firefox red**, ❌ Safari | ❌ | browsers ✅ | browser job ✅/✘ | `webrtc.rs:435`, `whep.spec.ts:36` |
| LL-HLS (fMP4, parts, blocking reload) | ✅ 19 packager tests | ✅ | ✅ string checks | ✅ hls.js Chromium/Firefox/WebKit-Linux | ⚠️ Apple validator **local only** | hls.js ✅, Apple ✅ (local) | E: macOS ✅; V: **never in CI** | `caudal-hls/src/tests.rs:226`, `e2e.rs:93`, `play.spec.ts:72` |
| Multi-rendition HLS / rendition reports | ✅ | ✅ ffprobe | ❌ | ❌ | ⚠️ validator skip in CI | ffprobe, Apple (local) | ❌ | `multi_rendition.rs:76,143` |
| Safari native HLS (macOS/iOS) | — | — | — | ⚠️ WebKit on **macOS only**, bimodal <5 s | ❌ real iPhone/iPad/Apple TV | Apple player (local) | ❌ Linux WebKit uses hls.js | `play.spec.ts:194` |
| MoQ output | ✅ | ⚠️ moq-native ↔ moq-native | ❌ (fingerprint only) | ✅ Chromium, Firefox | — | `@moq/watch` (same upstream) | browser ✅ but can SKIP silently | `moq.rs:48`, `moq.spec.ts:70` |
| RTSP server (TCP, UDP, RTSPS) | ✅ port pool, RTCP SR | ✅ ffprobe/ffmpeg | ❌ | — | ❌ VLC, GStreamer, NVR/VMS | ffmpeg ✅ | ❌ | `rtsp.rs:256,370,409` |
| RTSP pull (cameras) | ✅ reload diff | ⚠️ source is **Caudal's own server** | ❌ | — | ❌ ONVIF cameras, MediaMTX, GStreamer rtsp-server | none (circular) | ❌ | `rtsp.rs:309` |
| Transcoding (ffmpeg ladder) | ✅ | ✅ incl. crash restart | ❌ | ❌ | — | ffmpeg | ❌ | `caudal-transcode/tests/ffmpeg.rs:72,128,194` |
| Transcoding (rusty_h264) | ✅ decode check | ✅ | ❌ | ❌ | — | decoder | ❌ | `tests/rusty.rs:14` |
| Recording + VOD | ✅ meta | ✅ ffprobe | ⚠️ atom grep, no decode | ❌ VOD in a player | ❌ | ffprobe (crate level) | E macOS ✅ | `record.rs:135`, `e2e.rs:467` |
| Clips (progressive MP4) | — | ✅ ffprobe | ⚠️ `moov`/`stco` byte grep | ❌ | ❌ QuickTime/iOS Photos | ffprobe (crate) | ⚠️ | `e2e.rs:497` |
| Object-store upload (S3/GCS/R2) | — | ⚠️ `file://` only | ❌ | — | ❌ MinIO/S3 | none | ❌ | `record.rs:340` |
| 24/7 channels | ✅ shuffle | ✅ MP4 + TS, loop, skip | ❌ | ❌ UI Skip button | ❌ real-world files (VFR, edit lists, MKV) | ffmpeg-made fixtures | ❌ | `caudal-channel/tests/channel.rs:88–295` |
| Restream (RTMP/RTMPS push) | ✅ FLV, URL | ⚠️ into Caudal's own RTMP ingest | ❌ | ❌ Restreams screen live | ❌ YouTube/Twitch/nginx-rtmp | self | ❌ | `loopback.rs:102,310` |
| SCTE-35 (parse/build, TS, DATERANGE, RTMP onCuePoint) | ✅ | ✅ ffprobe sees PID | ✅ DATERANGE in e2e | — | ❌ threefive, TSDuck, SSAI vendor | ffprobe (presence only) | partial | `caudal-scte35/src/lib.rs:236–306`, `caudal-ts/tests/scte35.rs:122`, `e2e.rs:156` |
| Auth tokens (JWT HS256/JWKS) | ✅ 19 incl. `alg:none` | ✅ local JWKS server | ✅ publish+play | ❌ UI with tokens | ❌ tokens from a real IdP / other lib | jsonwebtoken mints **and** checks | macOS e2e | `caudal-auth/src/lib.rs:226–317`, `jwks.rs:84`, `e2e.rs:346,368` |
| Webhooks (Standard Webhooks) | ✅ | ✅ retries, 4xx no-retry | ❌ | — | ⚠️ `standardwebhooks` crate signs **and** verifies | same lib both sides | ✅ auth / **✘ health** | `hooks.rs:47`, `health/tests/integration.rs:85` |
| Admin login (password, sessions, CSRF, rate limit) | ✅ | ✅ axum oneshot | ⚠️ CLI only | ❌ Login screen never in a browser | — | self | ✅ (if reached) | `caudal-admin/tests/gate.rs:101–223`, `admin_cli.rs:14` |
| Admin OIDC (code + PKCE) | ✅ | ⚠️ hand-written mock IdP | ❌ | ❌ | ❌ Keycloak/Dex/Entra/Google | self mock | ✅ | `gate.rs:234,371–452` |
| Health alerts | ✅ rules, hysteresis | **✘ red in CI** | ❌ | ❌ | — | standardwebhooks | **red** | `health/src/rules.rs:187`, `tests/integration.rs:69` |
| Hot reload (SIGHUP, API) | ✅ diff/report | ✅ per-crate diff | ⚠️ restream + invalid only | — | — | self | **❌ never (gate)** | `reload.rs:34,79,91`, `subsystems.rs:307` |
| TLS / HTTP/2 / cert hot reload | — | ✅ rcgen + curl h2 | ✅ `https_speaks_http2` | ✅ WebKit over HTTPS | ⚠️ curl cross-check may skip | curl ✅ | ✅ | `caudal-tls/tests/tls.rs:142,198`, `e2e.rs:404` |
| ACME | ❌ | ❌ | ❌ | — | ❌ Pebble / LE staging | — | ❌ | STATUS "ACME untested against a real CA" |
| Metrics (Prometheus) | ✅ render, escaping | — | ⚠️ `contains("caudal_")` | — | ❌ `promtool check metrics` | self | ✅ | `caudal/src/metrics.rs:67–93`, `e2e.rs:37` |
| Config validation / `caudal check` | ✅ 10 | ✅ | ✅ line number | — | — | self | ✅ | `config.rs:784–884`, `e2e.rs:191` |
| Graceful shutdown with live viewers | — | ✅ TLS drain cap | ❌ SIGTERM with viewers | — | — | — | ❌ | `tls.rs:235`; `main.rs:104` |
| UI screens | ✅ vitest (api, webrtc, moq, format) | — | ✅ SPA fallback | ⚠️ Stream page only (HLS/WHEP/MoQ toggles) | ❌ Channels, Restreams, Recordings, Login, Publish | browsers | vitest **not in CI**; `dist/` never rebuilt in CI | `ui/app/src/*.test.ts`, `e2e.rs:280` |
| Static build (musl x86_64/aarch64) | — | — | ❌ binary never started | — | — | — | builds only | `ci.yml` build-static |
| Docker image | — | — | ❌ | — | — | — | **never built** | `Dockerfile` |
| Supply chain (licenses, advisories) | — | — | — | — | cargo-deny ✅ | RustSec ✅ | ✅ | `ci.yml` deny |

**Summary:** ingest and output paths have real I-level tests with ffmpeg on the other end, which
is good. The weakest areas are the Linux CI (red, so it verifies nothing), the external
validators (none of them run in CI), the device matrix (empty), the failure modes (see b) and
parser robustness (no fuzzing, and the release build aborts on panic).

---

## (b) Top 20 gaps, ranked by (chance a user hits it) × (damage)

H/M/L = likelihood × damage. "CI" = can GitHub-hosted CI run it.

| # | Gap | L×D | Concrete test to add | Tool | Layer | CI | Effort |
|---|---|---|---|---|---|---|---|
| 1 | **Linux CI is red, so nothing after `caudal-health` runs** | H×H | Diagnose `health/tests/integration.rs:69` on Linux (**to run:** `cargo test -p caudal-health --test integration` 20× on a 2-vCPU Linux box or Docker). Suspects: a lost wakeup between `count.load` and `notified()` (`:61–65`, fix with `Notify::notify_one`/permit or a watch channel); a single-threaded `#[tokio::test]` runtime starved by the delivery task; or a real bug when a publisher goes fully silent (no frames at all, not only no keyframes). Then add `--no-fail-fast`. | cargo, nextest | CI | yes | S |
| 2 | **Any panic takes down the whole server in release** (`panic = "abort"`), but tests only use debug + unwind | H×H | (a) Run the e2e suite against a **release** binary (or `CARGO_PROFILE_TEST_PANIC=abort` via `-Zpanic-abort-tests` on nightly) so a task panic kills the process and fails the test. (b) Make every e2e `Server` fail on drop if stderr contains `panicked at`. (c) Fuzz targets in #3. | cargo, harness | E | yes | S |
| 3 | **No fuzzing of network parsers** (PLAN rule broken) | M×H | `cargo-fuzz` targets: RTMP chunk stream + handshake (`vendor/scuffle-rtmp`), FLV/AMF `demux_video`/`demux_audio`/`parse_cue_point` (`caudal-rtmp/src/demux.rs`), `TsDemux::feed` + `Demuxer` (`caudal-ts`), `caudal_scte35::parse` + hex/base64 text, RTSP request/`Transport`/`parse_uri` (`caudal-rtsp/src/server.rs:80,436`), WHIP SDP offer → str0m, `avcc_nals`/`annexb_split` (`caudal-webrtc/src/codec.rs`), Opus TOC (`caudal-cmaf/src/opus.rs`), SRT streamid (`caudal-srt/src/connection.rs`), token extraction + `Authorizer::check`, MP4 channel source (`caudal-channel/src/source.rs`). Seed corpora from ffmpeg captures. | cargo-fuzz (libFuzzer), `arbitrary` | U | nightly job | M |
| 4 | **A publisher reconnect ends playback for every viewer.** Each publish gets a fresh `Packager` (`caudal-hls/src/lib.rs:148`), so an OBS network blip means ENDLIST, then a playlist whose media sequence restarts at 0. *LL-HLS closed 19 Sep 2026: a republish within `[hls] reconnect_grace_secs` (default 10) continues the playlist after an `EXT-X-DISCONTINUITY`; covered by `caudal-hls` unit tests, e2e `ll_hls_survives_a_publisher_reconnect` (ffmpeg + mediastreamvalidator) and `tests/browser/tests/reconnect.spec.ts` (Chromium, WebKit, Firefox). WHEP/MoQ still open.* | H×H | e2e: publish, open hls.js in Playwright, kill ffmpeg, republish within 2 s, then assert the player keeps playing within N s (or document the required player behaviour). Include `MEDIA-SEQUENCE` monotonicity across the republish and CDN-safe segment names. Same for WHEP/MoQ viewers. | Playwright + ffmpeg | B/E | yes | M |
| 5 | **A listener dies forever on one accept error.** `accept().await?` in `caudal-rtmp/src/lib.rs:37` and `caudal-rtsp/src/server.rs:218,248` (and the SRT accept at `caudal-srt/src/lib.rs:124`) returns, so with EMFILE, ECONNABORTED, etc. the ingest is gone until restart, and `/healthz` still says 200 | M×H | Integration test: set `RLIMIT_NOFILE` low in a child process (`ulimit -n 64`), open 100 idle TCP connections to the RTMP port, close them, then publish: the publish must succeed. Also check `/readyz` reflects a dead listener. | `ulimit`, Rust test | E | yes (Linux) | S |
| 6 | **No slow-client or idle-connection limits tested** (RTMP has no handshake timeout; no connection cap) | M×H | Slowloris: 1,000 TCP connects that never finish the RTMP handshake or RTSP request, then check fd count, RSS, and that a real publisher still gets in. Same for held LL-HLS blocking requests with a huge `_HLS_msn`. | Python/`tokio` script, `lsof` | E | yes | M |
| 7 | **Hot reload has never run in CI, and an RTMP listener restart probably races** (`subsystems.rs:359–363` aborts the old task and binds the same port at once, the error is only logged, and the API reports `restarted:["rtmp"]` with 200) | M×H | Enable `reload.rs` in CI (`CAUDAL_E2E=1 cargo test -p caudal --test reload`). Add: change only `[rtmp] app`, reload, publish to the new app and get it within 5 s; bind to a port in use, then the reload must answer non-2xx or `requires_restart`; 20 concurrent reloads (API + SIGHUP) while publishing. | e2e harness | E | yes | S |
| 8 | **The SRT suite never runs in CI** (all `SKIP`) | M×H | `apt-get install srt-tools` (Ubuntu ships `srt-live-transmit`), `brew install srt` on macOS; add `CAUDAL_REQUIRE_TOOLS=1` so a missing tool **fails** (see c). | apt/brew | CI | yes | S |
| 9 | **RTMP u32 timestamp wrap** (49.7 days), plus mid-stream backward jumps from encoders | M×H (24/7 users) | Unit + property: extend RTMP timestamps like `TsClock`, publish a synthetic FLV whose timestamps start at `u32::MAX - 5000`, then assert monotonic `dts`, no discontinuity in the HLS playlist, and that the ring window is not cut to one GOP (`caudal-core/src/stream.rs:157` computes the cutoff from `newest_micros`, which never goes backwards). | proptest, ffmpeg `-output_ts_offset` | U/I | yes | S |
| 10 | **No soak test** (memory, fds, task leaks, `publisher_lost` map growth, musl allocator) | M×H | Nightly 6 h: 20 RTMP publishers with random start/stop churn, 200 hls.js-equivalent pollers, WHEP clients joining and leaving. Sample RSS, fd count, tokio task count (`tokio-metrics`) and `/metrics` every minute. Fail if the slope is above a threshold. Run musl and mimalloc builds side by side. | custom Rust load tool or k6 + ffmpeg | E | self-hosted or 6 h GH job | M |
| 11 | **Apple validator and Safari native have never run in CI** | M×M | Self-hosted macOS runner (this Mac) with `mediastreamvalidator`, `CAUDAL_REQUIRE_HLS_VALIDATOR=1` and WebKit native over HTTPS. Validate HTTP/1.1 single-rendition **and** HTTPS + ABR, with a separate allow-list for each, because today `KNOWN_MUST` (`tests/support/mod.rs:310`) allows -50120/-50125 everywhere. | mediastreamvalidator, Playwright WebKit | V | self-hosted | S |
| 12 | **Firefox WHEP regression** (`videoWidth` stays 0) is open, and the browser tests never check pixels or audio | H×M | Bisect `68b4e2b..85d0370` (**to run**). Add a `requestVideoFrameCallback` frame counter, plus a canvas pixel-diff between frames (catches frozen video with advancing `currentTime`), plus a Web Audio `AnalyserNode` RMS > threshold on a `sine` source (nothing checks that audio is audible). | Playwright | B | yes | M |
| 13 | **Circular interop oracles** (str0m↔str0m, moq-native↔moq-native, RTSP pull from our own server, restream into our own RTMP) | M×M | Third-party endpoints: RTSP pull from **MediaMTX** and GStreamer `rtsp-server` (`gst-rtsp-launch`); restream into **nginx-rtmp**, MediaMTX, or `ffmpeg -listen 1 -f flv`; WHEP with GStreamer `whepsrc`; WHIP with GStreamer `whipsink` and OBS 30 (manual). | Docker images, GStreamer | I | yes (Docker on Linux) | M |
| 14 | **No real players/devices**: iPhone Safari, Android Media3/ExoPlayer, VLC, smart TVs, Shaka | M×M | Tier 1 (CI): VLC headless (`cvlc --play-and-exit --run-time 10` with `-vvv`, grep for decoder errors), GStreamer `playbin` (`gst-launch-1.0 ... ! fakesink` with timeouts), Shaka Player in Playwright. Tier 2 (weekly, manual or BrowserStack): iOS Safari, Android Media3 demo app, one Samsung/LG TV browser. | VLC, GStreamer, Shaka, BrowserStack | V/D | tier 1 yes | M |
| 15 | **Disk full or unwritable during recording; S3 failures** | M×M | Record into a 20 MB tmpfs/`hdiutil` RAM disk until ENOSPC; assert the live stream keeps flowing, the recording is marked failed/closed with ENDLIST, and no panic. Run MinIO in Docker with fault injection (`toxiproxy`: 500s, latency, drop), plus an HTTPS MinIO **from the `FROM scratch` image** (see "currently broken"). | tmpfs, MinIO, toxiproxy | I/E | yes (Linux) | M |
| 16 | **Graceful shutdown with live viewers** untested (only TLS drain cap) | M×M | e2e: 1 publisher, 5 HLS blocking pollers, 1 WHEP, 1 SRT viewer, recording on. Send SIGTERM and assert exit ≤ 6 s with code 0, recording closed with ENDLIST, `stream_ended` webhook delivered, no orphan ffmpeg (`pgrep`). | harness | E | yes | S |
| 17 | **No mutation testing**: we do not know whether the assertions actually pin the behaviour | M×M | `cargo mutants` first on `caudal-core/src/stream.rs`, `caudal-hls/src/packager.rs`, `caudal-auth/src/{lib,claims}.rs`, `caudal-admin/src/{http,session,ratelimit}.rs`, `caudal-health/src/rules.rs`, `caudal-ts/src/ts.rs`. Triage survivors into new asserts. Then `--in-diff` on PRs. | cargo-mutants | U | weekly job | M |
| 18 | **Open publishing on a public bind.** The start-up guard (`caudal-admin/src/config.rs:171`) only checks HTTP binds, so RTMP/SRT/WHIP on `0.0.0.0` with no `[auth]` lets anyone publish; also no auth-bypass tests at the HTTP layer | M×M | e2e: `[admin]` set, no `[auth]`, `rtmp.bind = 0.0.0.0`: expect a start-up warning or refusal (a decision for Saul). Bypass battery: `/api/v1/../`, `%2e%2e`, double slashes, `HEAD`/`OPTIONS` on admin routes, `X-Forwarded-For` spoofing against the rate limit, CSRF with a missing Origin, token in both header and query, oversized JWT. | Rust e2e, `ffuf`/`schemathesis` | E | yes | M |
| 19 | **Timing-sensitive LL-HLS invariants are only checked by example tests** | M×M | proptest on `Packager`: random frame streams (jitter, duplicate DTS, backward jumps, audio-only, B-frames, missing keyframes) → invariants: media sequence monotonic, every `EXTINF` ≤ `TARGETDURATION`, parts ≤ `PART-TARGET`, parts sum to their segment, `INDEPENDENT=YES` only on keyframe parts, DATERANGE IDs unique, playlist parses with `m3u8-rs` (second oracle). Note `packager.rs:381`: `d <= 0` (duplicate DTS) flushes and waits for a keyframe, so a source with repeated timestamps freezes video. That may be real, so test it. | proptest, m3u8-rs | U | yes | M |
| 20 | **Static binaries and the Docker image are never started** | M×M | In `build-static`: run `caudal --version` and `caudal check caudal.example.toml`, then start with a minimal config and hit `/healthz` (x86_64 native; aarch64 under `qemu-user`). Add a `docker build` + `docker run` smoke job that publishes one RTMP stream. Test the "refuses to start" message in the no-config Docker case. | qemu, docker | E | yes | S |

Next in line (21–25): ACME against **Pebble** (Let's Encrypt's test CA) in Docker; OIDC against
**Keycloak/Dex** containers; SCTE-35 cross-checked by **threefive** and TSDuck
`tsp -P tables --pid 0x1FFF`; TS output conformance with TSDuck `tsp -P continuity -P pcrverify`
and `tsanalyze`; SRT handshake amplification capture with `tshark` (BLINDSPOTS #10); `promtool
check metrics` on `/metrics`.

---

## (c) CI changes

### Fix now (this week)

1. **Get Linux green, then keep it honest:** `cargo test --workspace --no-fail-fast`, or better
   `cargo nextest run --workspace --no-fail-fast --retries 0` with JUnit output. Run nextest's
   `--retries 2` only in a separate job whose job is **reporting** flakes, never hiding them.
2. **Install the tools the tests need:** `sudo apt-get install -y ffmpeg srt-tools curl` on
   Ubuntu; `brew install ffmpeg srt` on macOS. Linux ffmpeg 6.1 has no WHIP muxer, so either
   use a static ffmpeg 8 build (johnvansickle/BtbN) or accept that WHIP is macOS-only and
   **say so** in the step summary.
3. **Turn every "SKIP" into a CI failure.** Add `CAUDAL_REQUIRE_TOOLS=1`. When it is set, every
   `have(...)`/`have_ffmpeg()`/`ffmpeg_has_whip()`/`have_validator()` check panics instead of
   returning. Put all these checks in one shared helper; today they are copied in 7 test files.
   Also print a summary of every `SKIP:`/`NOT VERIFIED:` line (`grep` the test output into
   `$GITHUB_STEP_SUMMARY`) and fail when the count goes above an allowed list.
4. **Run the env-gated suites:** replace
   `CAUDAL_E2E=1 cargo test -p caudal --test e2e` with
   `CAUDAL_E2E=1 cargo test -p caudal --tests -- --test-threads=1`, which also runs `reload.rs`
   and `admin_cli.rs`.
5. **Run a release binary once:** make the browser job's release build also run the e2e suite
   (`CAUDAL_BIN` override in `tests/support`), so a `panic = "abort"` crash fails CI.
6. **UI job:** `cd ui/app && npm ci && npm run lint && npm test && npm run build`, then
   `git diff --exit-code crates/caudal-ui/dist`, so the embedded UI cannot drift from its source.
7. **Browser job:** fail on `test.skip` in CI. The MoQ skip at `moq.spec.ts:71` hides a MoQ
   start-up failure behind a stale "agent P not merged" message.
8. **Sanity gates:** `cargo +1.95 check --workspace` (the stated MSRV), `gitleaks detect`,
   `cargo deny` (already there).

### New jobs

| Job | Trigger | What it runs | Runner |
|---|---|---|---|
| `validators` | push to main | Apple `mediastreamvalidator` (HTTP/1.1 + HTTPS/ABR), Safari native WebKit, VLC, GStreamer, TSDuck `tsp`/`tsanalyze`, threefive, `promtool` | **self-hosted macOS** (this Mac), labelled |
| `interop` | nightly | Docker: MediaMTX (RTSP source, RTMP/WHEP peer), nginx-rtmp (restream target), MinIO + toxiproxy (S3), Pebble (ACME), Keycloak or Dex (OIDC), GStreamer `whipsink`/`whepsrc` | ubuntu-latest |
| `fuzz` | nightly, 10 min per target | `cargo +nightly fuzz run <t> -- -max_total_time=600`; corpus kept in a `fuzz-corpus` branch or cache and uploaded as an artifact; any crash opens an issue | ubuntu-latest |
| `mutants` | weekly + `--in-diff` on PRs | `cargo mutants -p caudal-core -p caudal-hls -p caudal-auth -p caudal-admin -p caudal-health --timeout 120`; report missed mutants | ubuntu-latest |
| `soak` | nightly (6 h) + weekly (24 h, self-hosted) | load tool from gap #10 against the release musl binary; RSS/fd/task slopes; artifact with the time series | GH jobs cap at 6 h; 24 h self-hosted |
| `docker-smoke` | push to main | `docker build`, run with a minimal config, one RTMP publish, `/healthz`, HLS playlist through ffprobe | ubuntu-latest |
| `static-smoke` | in `build-static` | `--version`, `check`, start + `/healthz` (aarch64 via qemu-user) | ubuntu-latest |
| `ci-alive` | daily cron | fails (and emails) if the last `main` CI run is older than 24 h or was not started, which catches the silent billing outage | ubuntu-latest |

---

## (d) Tests that can't fail (or can't fail in CI)

| # | Where | Why it can't fail |
|---|---|---|
| 1 | `crates/caudal/tests/reload.rs:12–21` (all 3 tests) | Gated on `CAUDAL_E2E`. CI sets it only for `--test e2e`, so in CI every reload test returns early and passes. |
| 2 | `crates/caudal-srt/tests/srt.rs:230,335,355,381,404,453,476,503`; `crates/caudal/tests/e2e.rs:258` | `srt-live-transmit` is not installed in either CI job, so these always SKIP and pass. Confirmed in the macOS e2e log. |
| 3 | `crates/caudal/tests/e2e.rs:207–215` (validator canary), `e2e.rs:173–180`, `crates/caudal-hls/tests/multi_rendition.rs:144–146` | No `mediastreamvalidator` on GitHub runners, so `CAUDAL_REQUIRE_HLS_VALIDATOR=0` and the checks skip. The workflow warns, but the job stays green. |
| 4 | `crates/caudal-webrtc/tests/webrtc.rs:163–166`, `crates/caudal/tests/e2e.rs:448–450` | The WHIP tests SKIP on Ubuntu's apt ffmpeg (6.1, no WHIP muxer). |
| 5 | `crates/caudal-tls/tests/tls.rs:185–186` | The curl HTTP/2 cross-check only logs and continues when curl fails or is missing. |
| 6 | `crates/caudal-rtsp/tests/rtsp.rs:257,371,410`, `crates/caudal-hls/tests/multi_rendition.rs:123`, `crates/caudal-ts/tests/scte35.rs:125`, `crates/caudal-rtmp/tests/rtmp.rs:*` (`have_ffmpeg`/`have_libx265`) | They pass whenever ffmpeg, ffprobe or libx265 is missing. That is fine on today's runners, but the protection only lasts as long as someone remembers to keep the tools installed. |
| 7 | `tests/browser/tests/moq.spec.ts:71–76` | Skips (it does not fail) whenever `/moq/fingerprint` is not JSON, so a MoQ start-up failure looks like "not merged yet". |
| 8 | `tests/browser/tests/{play,steady,whep,moq}.spec.ts:28/34/11/27` | The whole suite skips if ffmpeg is missing. |
| 9 | `crates/caudal/tests/e2e.rs:81–91` `wrong_app_name_is_rejected` | Only asserts that the stream is absent (404) after 3 s. There is no positive control, so it also passes if ffmpeg never connected, the port was wrong, or ingest is broken. Assert that the connection was **closed by the server** (ffmpeg exit code or the time until EOF), the way `rtmp.rs:206` does. |
| 10 | `crates/caudal-srt/tests/srt.rs:345–349` (`play/` streamid), `:372–376` (no passphrase) | A negative check after a `sleep`, with no proof the caller reached the listener. It passes if the pipeline never started. |
| 11 | `crates/caudal-transcode/tests/ffmpeg.rs:175–190` | "Renditions never transcoded" = sleep 2 s, then check that nothing appeared. It is vacuous if the transcoder task never started. Add a positive control (a matching source in the same run *does* produce a rendition). |
| 12 | `crates/caudal/tests/e2e.rs:497–503` | Clip validity = byte-grep for `ftyp`/`moov`/`stco`. A corrupt MP4 passes. Run `ffprobe -v error` on it and decode it (the crate test does; the e2e test doesn't). |
| 13 | `crates/caudal/tests/e2e.rs:37` | `/metrics` "has our metrics" = `contains("caudal_")`. Run `promtool check metrics`, and assert on a named series with a stream label. |
| 14 | `tests/browser/tests/play.spec.ts:190–196`, `steady.spec.ts:225–229` | WebKit native only has to be < 5 s. A regression from 0.5 s to 4.9 s passes as "known-gap". Track the rate of low-latency joins and alert when it drops. |
| 15 | `tests/browser/tests/whep.spec.ts:41–48`, `play.spec.ts:176–178` | "Plays" = `readyState`, `videoWidth`, `currentTime` advancing. A frozen or black frame with a running clock passes. Nothing checks audio. |
| 16 | `crates/caudal/tests/support/mod.rs` `KNOWN_MUST` (l.310–319) | -50120 and -50125 stay allow-listed, although STATUS says both are fixed on the HTTPS/ABR path. A regression there would only show on a run that is never in CI. |
| 17 | `.github/workflows/ci.yml` `cargo test --workspace` | No `--no-fail-fast`: one red crate hides the pass/fail state of every later crate (happening now). |
| 18 | Probe-and-release port helpers: `caudal-rtsp/tests/rtsp.rs:24,34`, `caudal-hls/tests/multi_rendition.rs:21`, `caudal-restream/tests/loopback.rs:35`, `caudal-rtmp/tests/rtmp.rs:26`, `caudal-srt/tests/srt.rs:25`, `crates/caudal/tests/support/mod.rs:20,24`, `tests/browser/tests/harness.ts:20,39` | Not "can't fail" but "fails randomly", and against STATUS's own hygiene rule (l.20). A likely cause of the `h264_aac_publish_end_to_end` flake. |
| 19 | Circular oracles: `caudal-webrtc/tests/webrtc.rs:435` (str0m↔str0m), `caudal-moq/tests/moq.rs:48` (moq-native↔moq-native), `caudal-rtsp/tests/rtsp.rs:309` (pull from our own server), `caudal-restream/tests/loopback.rs:102` (push into our own ingest), `caudal-auth/tests/hooks.rs:47` + `health/tests/integration.rs` (same `standardwebhooks` crate signs and verifies), JWT tests (jsonwebtoken mints and checks) | A bug shared by both ends, or a spec misreading shared by both, passes. The scuffle Type-3 chunk bug is exactly this class. |

---

## (e) Phased plan

### Phase 1: this week (make CI tell the truth; the cheapest high-value checks)

1. Diagnose and fix `caudal-health/tests/integration.rs` on Linux (**to run** on Linux: loop it
   20×). Switch to `nextest --no-fail-fast`. *(gap 1)*
2. CI installs `srt-tools`; add `CAUDAL_REQUIRE_TOOLS=1` with one shared `require(tool)`
   helper; add a SKIP summary. *(gaps 8, d2–d8)*
3. Run `reload.rs` and `admin_cli.rs` in CI; add the RTMP-listener-restart reload test and the
   bind-in-use reload test. *(gap 7)*
4. Replace every probe-and-release port helper with bind-0-and-pass-the-socket or retry-on-exit.
   *(d18)*
5. e2e on the **release** binary + fail on `panicked at` in server stderr. *(gap 2)*
6. The accept-error test with a low `ulimit -n`, then fix the three `accept().await?` loops.
   *(gap 5)*
7. The RTMP wrap test (it fails today), then the fix. *(gap 9)*
8. Register this Mac as a self-hosted runner for the `validators` job:
   `CAUDAL_REQUIRE_HLS_VALIDATOR=1`, WebKit native over HTTPS, VLC and GStreamer playback smoke.
   *(gaps 11, 14 tier 1)*
9. Bisect the Firefox WHEP regression; add the frame-counter and audio-RMS assertions. *(gap 12)*
10. UI job (lint, vitest, build, `git diff --exit-code dist`), plus the `ci-alive` cron.

### Phase 2: next two weeks (robustness: fuzzing, failure modes, independent interop)

1. `fuzz/` with the ~11 targets from gap 3, a nightly job, and saved corpora. **First:** RTMP
   chunk + FLV/AMF, TS demux, SCTE-35, RTSP request, WHIP SDP.
2. proptest: ring buffer model (random push/subscribe/lag/end → memory bound, joins on
   keyframes, ingest never blocks), LL-HLS `Packager` invariants (gap 19), RTMP/TS timestamp
   extension, FLV mux↔demux and TS mux↔demux round trips, SCTE-35 build↔parse (+ threefive as
   the external oracle).
3. The publisher-reconnect browser test and the decision it forces (keep the timeline across a
   republish within N s?). *(gap 4)*
4. `interop` nightly: MediaMTX as the RTSP camera, nginx-rtmp as the restream target, GStreamer
   `whipsink`/`whepsrc`, Keycloak/Dex for OIDC, Pebble for ACME, MinIO + toxiproxy for S3
   (including from the Docker image). *(gaps 13, 15)*
5. The failure-mode e2e set: SIGTERM with live viewers (gap 16), disk full while recording
   (gap 15), ffmpeg killed mid-ladder under load (extends `ffmpeg_crash_is_restarted` to the real
   binary), slowloris and idle connections (gap 6), the auth bypass battery (gap 18).
6. `docker-smoke` and `static-smoke`. *(gap 20)*
7. TSDuck `continuity`/`pcrverify`/`tables` and threefive on the SRT and TS output in the
   validators job.

### Phase 3: this month and after (long runs, simulation, devices)

1. `soak` nightly 6 h on GH + weekly 24 h self-hosted, musl vs mimalloc RSS. *(gap 10,
   BLINDSPOTS #11)*
2. `cargo mutants` weekly on the core crates, `--in-diff` on PRs; drive missed mutants to zero in
   `caudal-core` and `caudal-auth`. *(gap 17)*
3. `turmoil` 0.7.2 simulations for the network state machines with reconnect and retry logic:
   restream push reconnect, `[[srt.push]]`, RTSP pull reconnect (1→30 s backoff), webhook
   delivery retries, JWKS refetch, OIDC provider down. Deterministic partitions and latency
   instead of `sleep`. `loom` (or `shuttle`) only on a small extracted model of
   `caudal-core`'s ring + wake path (`stream.rs:278–302`, subscriber cursor); the rest of the
   codebase is ordinary tokio code where loom does not pay off.
4. Real-device pass, written down as a checklist with screenshots and numbers: OBS 30 (RTMP,
   E-RTMP HEVC, SRT, WHIP), vMix or one hardware encoder, an iPhone (Safari native, including
   Opus-in-HLS: BLINDSPOTS #3), an Android phone (Media3 demo), a smart-TV browser, one ONVIF
   camera for RTSP pull, VLC for the RTSP server over UDP. Repeat per release.
5. Capture-based checks: `tshark` on RTMP (chunk sizes, timestamps across a wrap with a
   synthetic source), SRT spoofed induction (amplification), RTSP interleaved; keep pcaps as
   regression fixtures that replay into the fuzz corpora.
6. 1,000-viewer LL-HLS blocking-reload herd test from the benchmark harness, kept as a
   regression bound (BLINDSPOTS #5).
7. Coverage (`cargo llvm-cov`) as a map of untested code, never as a target.

---

## Probably broken today, with no test that would show it

Each of these comes from reading the code. None has been confirmed by a run.

1. **Linux CI** fails at `caudal-health/tests/integration.rs:69`, in two consecutive runs.
2. **Firefox WHEP** plays no video (known, open, red in the browser job).
3. **One accept error kills a listener for good:** RTMP (`caudal-rtmp/src/lib.rs:37`), RTSP
   (`caudal-rtsp/src/server.rs:218,248`) and SRT (`caudal-srt/src/lib.rs:124`) all return on the
   first accept error (EMFILE under load, for one). `/healthz` stays 200.
4. **Any task panic aborts the process in release** (`Cargo.toml:53`). The comment at
   `caudal-rtmp/src/lib.rs:42–43` ("a panic … in one never affects any other publisher") is
   true only in debug builds.
5. **Reload of `[rtmp]` with the same bind can race:** the old accept task is aborted and the
   new one binds at once (`subsystems.rs:359–363`). If the old socket is not dropped yet, the
   bind fails, and that is only logged while the API reports `restarted`.
6. **A publisher reconnect ends every HLS viewer's session:** a fresh packager restarts the media
   sequence at 0 after ENDLIST (`caudal-hls/src/lib.rs:148`).
7. **RTMP timestamp wrap at 49.7 days** (`caudal-rtmp/src/demux.rs:52`), already known. Likely
   effect beyond the discontinuity: the ring's window cutoff uses a `newest_micros` that never
   goes backwards (`caudal-core/src/stream.rs:294,157`), so after any backward jump the DVR
   window shrinks to one GOP.
8. **Duplicate or zero-delta video DTS** is treated as a jump (`caudal-hls/src/packager.rs:381`):
   video freezes until the next keyframe and a discontinuity is written.
9. **S3/GCS upload from the `FROM scratch` Docker image over HTTPS** probably fails certificate
   verification. `caudal-record` uses reqwest 0.13 with `rustls-no-provider`
   (`crates/caudal-record/Cargo.toml:23`), which by default verifies through the platform
   verifier and needs a system CA store, and the image ships none (`Dockerfile`). reqwest 0.12
   in `caudal-auth`/`caudal-health` uses bundled webpki roots and should be fine. **To run:**
   Docker image + MinIO over TLS, or a real bucket.
10. **Anyone can publish** over RTMP/SRT/WHIP on a public bind without `[auth]`. The start-up
    guard only looks at HTTP binds. This may be by design, but no test or doc covers it.
