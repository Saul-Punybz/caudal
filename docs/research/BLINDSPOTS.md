# Blind spots: Gemini review, checked by hand

Prompt: `gemini-blindspots-prompt.md`. Answer received 19 Sep 2026 and checked the
same day against crates.io (API) and Caudal's own code. Nothing below is taken on
Gemini's word; each row says what was checked.

## Scorecard

- **Claims about crates:** 5 of 9 versions wrong, 1 crate invented (`srtrust`).
- **Claims about Caudal:** 4 were already done or false (ffmpeg in-process, TS PTS
  wrap, SRT push, `str0m` crypto path), 1 was right for the wrong reason (allocator:
  the static builds use **musl**'s allocator, not glibc's).
- **Useful:** 8 items worth doing, plus 1 real bug it pointed near but did not find
  (RTMP 32-bit timestamp wrap).

## Crate facts (crates.io, 19 Sep 2026)

| Crate | Gemini said | Actual | Notes |
|---|---|---|---|
| quinn | 0.11.16, Jul 2026 | **0.11.12**, 14 Sep 2026 | wrong version |
| quinn-udp | 0.6.1 | **0.6.2**, 6 Sep 2026 | GSO/GRO support is real |
| s2n-quic | 1.86.0 | **1.88.0** | Apache-2.0 |
| rubato | 3.0.0 | **5.0.0**, 10 Aug 2026 | wrong major |
| str0m | 0.14.0 | **0.23.1** | Caudal already uses 0.23.1 with `rust-crypto` |
| str0m-rust-crypto | 0.3.0 | **0.6.0** | wrong |
| turmoil | 0.6.6, Mar 2025 | **0.7.2**, Apr 2026 | wrong |
| scte35-splice | 2.1.0 | 2.1.0 | correct (in use) |
| ktls | 6.0.2, Apr 2025 | 6.0.2, 7 Apr 2025 | correct; no release in 17 months |
| srtrust | "pure Rust SRT" | **does not exist** | invented |
| ebur128 | (not named) | 0.1.10, MIT, sdroege | pure-Rust EBU R128, found while checking #13 |

## Findings, verdicts

| # | Gemini's finding | Verdict | Evidence | Action |
|---|---|---|---|---|
| 1 | In-process ffmpeg can segfault the server | **False.** ffmpeg already runs as a child process; the in-process path is pure Rust (`rusty_h264`) | `crates/caudal-transcode/src/ffmpeg.rs:318` (`Command::new`) | none |
| 2 | UDP GSO/GRO missing for WebRTC/SRT | **True.** WebRTC does one `recv_from`/`send_to` per datagram on a plain tokio socket | `crates/caudal-webrtc/src/engine.rs:121` | measure after the MediaMTX benchmark; `quinn-udp` 0.6.2 for batched I/O |
| 3 | No pure-Rust AAC encoder | **True** (none found on crates.io; `fdk-aac` binds C). Premise overstated: WHIP Opus is packaged into fMP4 as Opus, and AAC transcoding goes through the external ffmpeg process | `crates/caudal-hls/src/mp4demux.rs:73` | verify Opus-in-HLS playback on Safari/iOS with a real device; not verified |
| 4 | 33-bit PTS wrap breaks 24/7 streams | **False for TS** (`TsClock` extends to 64-bit). **But a real neighbour:** RTMP timestamps are `u32` ms and go straight to `i64` with no unwrap, so a publish longer than 2^32 ms (**49.7 days**) jumps back to 0 | `crates/caudal-ts/src/demux.rs:48`; `crates/caudal-rtmp/src/demux.rs:52` | **fix: extend RTMP timestamps like `TsClock`**, with a test across the wrap |
| 5 | LL-HLS thundering herd on blocking reloads | **Unverified.** Plausible at 1,000+ viewers | none yet | read the benchmark's 1,000-viewer LL-HLS numbers first |
| 6 | Binary upgrade drops viewers (no socket handoff) | **True.** Hot reload covers config, not the binary | `crates/caudal/src/subsystems.rs` | backlog (L): systemd socket activation / fd passing |
| 7 | SSRF via pull URLs and webhooks | **Partly.** Every URL comes from the TOML file (admin-only); no API accepts a URL today | `grep url` in `api.rs`, restream/channel `http.rs`: none | before any API that takes URLs (multi-tenancy), add a private-range/metadata deny-list |
| 8 | SSAI needs legacy `EXT-X-CUE-OUT/IN` | **True, not implemented** (it was in our own research order in `SCTE35.md`) | no `CUE-OUT` in `crates/caudal-hls/src` | add `[hls] cue_tags = "daterange" / "cue-out" / "both"` |
| 9 | WASM in the frame path breaks zero-copy | **Fair.** PLAN lists "frame-level filters" | `PLAN.md` backlog | WASM for control plane first; frame filters only with a measured cost |
| 10 | UDP amplification via STUN/SRT | **WebRTC: false.** Datagrams no session accepts are dropped. **SRT: unverified** (depends on `rsrt`'s handshake cookie) | `crates/caudal-webrtc/src/engine.rs:133` | check SRT with a spoofed-induction capture in `tshark` |
| 11 | Allocator fragmentation (glibc) | **True for the wrong reason.** Release/Docker builds are static **musl**, whose allocator is the concern | `Dockerfile:12` | measure RSS over a long run with musl vs mimalloc before switching |
| 12 | Cancellation safety of writes in `select!` | **Already found and fixed once** (restream); a grep finds no other `select!` arm that writes | `crates/caudal-restream/src/push.rs` | keep; add `turmoil` 0.7.2 simulation tests later |
| 13 | EBU R128 loudness normalization | **Plausible market need, pure-Rust path exists** (`ebur128` 0.1.10) | crates.io | backlog; needs decode + gain + encode, so it rides the transcode path |
| 14 | kTLS for HTTPS delivery | **True idea, Linux only.** `ktls` 6.0.2 has no release since Apr 2025 | crates.io | after the benchmark shows TLS cost |
| 15 | "SRT push is king", implement it | **Already done:** SRT listener ingest and `[[srt.push]]` caller output exist | `crates/caudal-srt` | none |
| — | OMT is scope creep | **Speculative**, and a product decision Saul already made (18 Sep 2026) | — | none |
| — | Behind a CDN, LL-HLS needs exact cache keys/headers | **Plausible**, no evidence given | — | document tested CDN settings when one is tested |

## New work that came out of this (in priority order)

1. **Bug:** RTMP timestamp wrap at 49.7 days (`caudal-rtmp/src/demux.rs:52`).
2. `[hls] cue_tags` with legacy `EXT-X-CUE-OUT/IN` for SSAI vendors.
3. SRT handshake amplification check with a packet capture.
4. Long-run RSS: musl allocator vs mimalloc.
5. Batched UDP I/O (GSO/GRO) for WebRTC/RTSP-UDP/SRT, after the benchmark.
6. Opus-in-HLS playback on a real Apple device.
7. Backlog: socket handoff for binary upgrades; kTLS; R128 loudness; SSRF deny-list before URL-taking APIs; `turmoil` simulation tests.
