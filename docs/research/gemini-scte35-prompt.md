# Gemini Deep Research prompt: SCTE-35 building blocks for Caudal

Copy everything below the line into Gemini Deep Research.

---

You are a senior broadcast/streaming engineer doing due diligence for an open-source live media server called **Caudal**, written in pure Rust (tokio, no C or Go libraries linked; external processes like ffmpeg are allowed but not preferred). License of Caudal: MIT OR Apache-2.0, so every component we reuse must be license-compatible (MIT, Apache-2.0, BSD, ISC, Zlib, Unlicense are fine; GPL/AGPL/LGPL are NOT, unless only used as a reference to read, never copied).

**What we are building (SCTE-35 support):**
1. **Passthrough:** read SCTE-35 cues (table_id 0xFC, `splice_info_section`) from incoming MPEG-TS (SRT, RTSP-in-TS) and from RTMP/E-RTMP (AMF `onCuePoint`/`onAdCue`-style data messages), keep them aligned with media timestamps (pts_adjustment, 33-bit PTS wrap).
2. **Output mapping:**
   - LL-HLS: `#EXT-X-DATERANGE` with `SCTE35-OUT` / `SCTE35-IN` / `SCTE35-CMD`, `PLANNED-DURATION`, `CUE-OUT`/`CUE-IN`; also the legacy `#EXT-X-CUE-OUT` / `#EXT-X-CUE-OUT-CONT` / `#EXT-X-CUE-IN` tags; split segments/parts on the splice point.
   - CMAF / DASH: `emsg` boxes (scheme `urn:scte:scte35:2013:bin`) and MPD `EventStream`.
   - MPEG-TS out (SRT push): re-mux the section on its own PID, listed in the PMT with stream_type 0x86 and the CUEI registration descriptor.
3. **Insertion:** our 24/7 channel (a playlist of files played as a live stream) must *generate* cues: `splice_insert` and `time_signal` + `segmentation_descriptor` (provider/distributor placement opportunities, break start/end), from a schedule or an API call.
4. **Validation:** a way to test all of this automatically (reference streams, decoders, validators).

**We already evaluated:**
- `scte35-splice` 2.1.0 (crates.io, repo github.com/fishloa/rust-broadcast), parser + builder, ANSI/SCTE 35 2023r1, forbid(unsafe), 102 tests. Risk: single author, repo created June 2026, 1 star, renamed from `dvb-scte35`.
- `scte35-reader` 0.16 (dholroyd, parse-only), `scte35` 0.2 (rafaelcaricio), `scte35dump`, `mp4-emsg` 0.4, `dash-mpd` 0.20.
- Our TS demux/mux is our own crate built on `mpeg2ts` / `mpeg2ts-reader`.

**Research questions. For each item give: exact repo URL, crates.io/npm/PyPI name, language, license (SPDX, as stated in the repo's LICENSE file), last commit date, stars, number of contributors, test coverage signals, and whether it parses, builds, or both.**

1. Every **Rust** crate or repo handling SCTE-35 (splice_info_section, segmentation descriptors, SCTE-104, SCTE-224 ESNI, SCTE-35 in HLS/DASH), including forks and variations of `scte35-splice` / the `rust-broadcast` family (e.g. `dvb-scte35`, sibling crates for DVB/TS tables), and Rust HLS/DASH manifest crates that already model `EXT-X-DATERANGE` or `EXT-X-CUE-OUT` (e.g. `m3u8-rs`, `hls_m3u8`, others). Compare them in one table and recommend a primary and a fallback.
2. The best **non-Rust** reference implementations we could read and port (not link): e.g. threefive / threefive3 (Python), scte35-js / scte35.js (JS/TS), Comcast `scte35-go` / `gots`, GPAC, Shaka Packager, Bento4, TSDuck, Wowza/AWS Elemental docs. For each: license, what exactly it gets right that Rust crates miss (segmentation_upid types, encryption, splice_schedule, DTMF, time_signal vs splice_insert handling, HLS tag styles).
3. How **Apple, AWS MediaTailor, Google DAI, Broadpeak, and Yospace** expect SCTE-35 in LL-HLS and DASH (which tag styles, which fields are mandatory, how cues must align with segment/part boundaries, CUE-OUT duration rules, "IN" signaling). Cite the spec or official doc for each claim. Also: does Apple's `mediastreamvalidator` check DATERANGE/SCTE-35 tags, and which errors does it emit?
4. How **OvenMediaEngine, MediaMTX, SRS, Nimble Streamer, Flussonic, Wowza, and MistServer** handle SCTE-35 today (passthrough? insertion API? which outputs?). Link to docs or source files.
5. **Test material**: public SCTE-35 sample TS files, conformance streams, and tools to generate cues (e.g. TSDuck `tsp` with `spliceinject`, ffmpeg's support or lack of it, threefive's encoder), with licenses.
6. RTMP: which ingest encoders (OBS, vMix, Wirecast, AWS Elemental Live, Haivision) send SCTE-35 over RTMP, in which AMF message shape, and whether Enhanced RTMP (v2) defines anything for it.
7. Anything we have not considered: SCTE-35 security (encrypted cues), SCTE-250, the 2023 split into SCTE 35-1 / 35-2, SGAI (server-guided ad insertion, HLS interstitials `EXT-X-DATERANGE CLASS="com.apple.hls.interstitial"`), and which of these matter for FAST channels in 2026.

**Rules for your answer:**
- Verify every repo link resolves and every license from the repo's own LICENSE file. If you cannot verify something, write "unverified" next to it. Do not invent crates, versions, or star counts.
- Separate "use as a dependency" from "read as a reference" clearly, and flag anything GPL/AGPL/LGPL as reference-only.
- End with a recommended architecture for Caudal: which crate(s) to depend on, which pieces to write ourselves, and the order to build them (passthrough first, then HLS mapping, then CMAF/DASH, then insertion), with the risks of each choice.
- Format: markdown, tables where possible, under 2,500 words.
