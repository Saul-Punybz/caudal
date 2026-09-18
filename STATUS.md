# STATUS — Caudal

**Last updated:** 17 Sep 2026

## What it is
Open-source rewrite of MistServer in Rust. Full plan and evidence in `PLAN.md`; reuse inventory in `REUSE.md`.

## Where we are
- **M0 done:** `crates/caudal-core`: the media model (tracks, frames on their native clock) and the live buffer (one publisher, many viewers; slow viewers skip to a keyframe; memory bounded by time and bytes). 13 tests pass, clippy clean.
- `crates/caudal`: empty binary for now.

## Finding, 17 Sep (evening)
SRT and RIST **do exist in pure Rust**: `rsrt` (cesbo, verified against libsrt 1.5.6: 668 tests + interop) and `rist-core` (wavey-ai, Simple + Main profiles, interop against librist). Details in `REUSE.md`. No need to port gosrt or libRIST.

## Finding, 18 Sep (early)
Three Sonnet agents surveyed backlog tiers A, B and C; every number re-verified by hand. Full tables in `REUSE.md`. Biggest win: **moq-dev/moq already has `moq-mux`, `moq-hls`, `moq-rtmp` (enhanced RTMP), `moq-srt` and `moq-relay`**. M1 starts with an evaluation of building on `hang` + `moq-mux`.

## Next
**M1: the server shell.** TOML config, HTTP API (axum) listing streams and their stats, Prometheus `/metrics`, graceful shutdown. Then **M2 + M3:** RTMP in and LL-HLS out, the first path you can watch in a browser.

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
