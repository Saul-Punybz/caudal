# SCTE-35 for Caudal: verified research

Gemini Deep Research report (prompt: `gemini-scte35-prompt.md`), received 18 Sep 2026, checked the same day against GitHub, crates.io and code search.

## Corrections to Gemini (verified)

| Gemini | Reality |
|---|---|
| `dvb-scte35` is a "newer (June 2026) no_std rewrite" and an alternative | It is the **old name** of `scte35-splice` (7.9.x); per the README it is deprecated and re-exports `scte35-splice`. Same author. Not an alternative. |
| `scte35-reader`: MIT, 34 stars | Apache-2.0 on GitHub (crate metadata says MIT/Apache-2.0), 8 stars, last push Apr 2026. |
| `m3u8-rs` is `theRealRobG/m3u8` (MIT, 24 stars) | `m3u8-rs` is `rutgersc/m3u8-rs` (MIT, 125 stars, 1.56M downloads). `theRealRobG/m3u8` is a different project (Unlicense, 4 stars). |
| `hls_m3u8`: MIT/Apache | Apache-2.0 on GitHub; 66 stars; has `EXT-X-DATERANGE` (code search, 9 hits). |
| Avoid TSDuck (GPL/LGPL) | TSDuck is **BSD-2-Clause** (1,085 stars, active): usable as reference and as a test tool. GPAC is the LGPL one. |
| `futzu/threefive3` | Repo not found. `futzu/threefive` exists: MIT, 163 stars, **last push Nov 2024**. |
| `Comcast/gots`: MIT | GitHub reports NOASSERTION; treat as reference only until the LICENSE file is read. `Comcast/scte35-go`: Apache-2.0, active (Sep 2026). |
| MediaMTX relies on "Ant Media's SSAI plugin" | No evidence; code search finds no SCTE-35 code in MediaMTX. Say only "MediaMTX has no SCTE-35 support". |
| MistServer "recently added basic passthrough" | Confirmed SCTE-35 code in `lib/ts_stream.*`, `lib/ts_packet.cpp`, `src/output/output_httpts.cpp`, `output_json.cpp` (TS-level). Date not verified. |
| SESAME = "SCTE 130-9 AES-GCM encryption", crate `sesame-esam` | `sesame-esam` exists (0.1.3, 143 downloads). The spec claim is **unverified**; not pursued. |
| `mediastreamvalidator` checks DATERANGE alignment; Google DAI carries CUE-OUT in ID3 | **Unverified.** We will test the validator ourselves (we run it in CI). |
| Splitting a segment on an ad break requires `EXT-X-DISCONTINUITY` | Only when timestamps actually jump (e.g. an ad is stitched in). Passthrough of cues on continuous content does not need it. |

## Confirmed and adopted

- SCTE35-OUT/IN/CMD in `EXT-X-DATERANGE` carry the `splice_info_section` as hex (RFC 8216 §4.3.2.7.1).
- Default to **`time_signal` + `segmentation_descriptor`** when Caudal generates cues (SSAI platforms prefer it); `splice_insert` on request.
- **HLS Interstitials** (`EXT-X-DATERANGE CLASS="com.apple.hls.interstitial"`, `X-ASSET-URI`) are real (Apple HLS spec, 2024) and matter for FAST channels: add after the SCTE-35 mapping.
- RTMP: cues arrive as AMF0 data messages (`onCuePoint` / `onAdCue` styles); Enhanced RTMP defines nothing for SCTE-35.
- 33-bit PTS wrap handling is ours to get right in `caudal-ts` / core.

## Decision

| Piece | Choice |
|---|---|
| Parse + build `splice_info_section` | `scte35-splice =2.1.0` behind our own `scte35` module (swappable). Fallback parse: `scte35-reader`. |
| HLS playlists | **Keep our own packager writer** (it is Apple-validated); add DATERANGE there. `hls_m3u8` / `m3u8-rs` only as test-side parsers. |
| CMAF/DASH | `mp4-emsg` for `emsg`; `dash-mpd` when DASH output lands. |
| References (read, not link) | `Comcast/scte35-go` (Apache-2.0), `futzu/threefive` (MIT), `Eyevinn/hls-m3u8` (BSD-3), TSDuck (BSD-2). |
| Test tool | TSDuck `tsp` + `spliceinject` to inject cues into TS for ingest tests (gated on it being installed). |

Order: (1) TS + RTMP ingest → cues in core with PTS; (2) LL-HLS DATERANGE (+ legacy CUE-OUT/IN option), TS out on its own PID (stream_type 0x86, CUEI); (3) insertion API + 24/7 channel breaks; (4) CMAF `emsg`, HLS Interstitials.
