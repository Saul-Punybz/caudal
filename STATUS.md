# STATUS — Caudal

**Last updated:** 18 Sep 2026

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
| 2 | RTMP ingest → `Publisher` (`caudal-rtmp`) | TODO | 1 batch |
| 3 | LL-HLS packager + `/play/{name}` page (`caudal-hls`) | TODO | 1 batch |
| 4 | Server shell: TOML config, `/api/v1/streams`, `/metrics`, `/healthz`, graceful shutdown (`caudal`) | TODO | 1 batch |
| 5 | Wiring in `main.rs`, CI, `FROM scratch` Dockerfile | TODO | orchestrator + ½ batch |
| 6 | Open with OBS, watch in Safari/Chrome, measure latency | TODO | close of batch 1 |

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

## Next
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
