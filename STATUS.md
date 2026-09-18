# STATUS — Caudal

**Last updated:** 18 Sep 2026 (batch 3 closed except the Safari measurement)

## What it is
Open-source rewrite of MistServer in Rust. Full plan and evidence in `PLAN.md`; reuse inventory in `REUSE.md`.

## Where we are
- **M0 done:** `crates/caudal-core`: the media model (tracks, frames on their native clock) and the live buffer (one publisher, many viewers; slow viewers skip to a keyframe; memory bounded by time and bytes). 13 tests pass, clippy clean.
- `crates/caudal`: empty binary for now.

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
