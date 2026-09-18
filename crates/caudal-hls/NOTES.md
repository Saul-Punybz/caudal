# caudal-hls notes (batch 1, agent C)

## Layout
- `src/lib.rs`: `router`, the publish listener, one packager task per stream, HTTP handlers, blocking waits (`watch` channel bumped when a part/segment/init appears).
- `src/packager.rs`: synchronous segmenter (frames in, parts/segments/playlist out). No clock inside; the task stamps `SystemTime::now()` on each frame.
- `src/fmp4.rs`: `ftyp`+`moov` (avc1/avcC, hvc1/hvcC, mp4a/esds, Opus/dOps) and `moof`+`mdat` via `mp4-atom` 0.15.
- `src/opus.rs` (batch 4, agent N): parses RFC 7845 `OpusHead` (little-endian) out of `TrackInfo::init`, and reads a packet's duration off its TOC byte (RFC 6716 §3.1) instead of assuming 20 ms. `mp4-atom` 0.15 already ships `Opus`/`Dops` sample-entry types (`src/moov/trak/mdia/minf/stbl/stsd/opus.rs` in the crate source), so `fmp4.rs` only converts `OpusHead` fields into a `Dops` box (its `encode_body` always writes `ChannelMappingFamily = 0`, so we refuse to build an init segment when the source's OpusHead declares a non-zero mapping family, same as we refuse a config record we can't parse for AAC/H.264/H.265).
- `src/mp4demux.rs`: small progressive-MP4 demuxer used only by the tests and `examples/demo.rs`. Not compiled into the library. It rebuilds `OpusHead` bytes from an mp4-atom `Dops` box itself (a few duplicated lines, not `crate::opus`) because this file is compiled standalone into two different crates (the test binary and the `demo` example) and `opus` is a private module.
- `tests/fixtures/av.mp4`: 4 s, 256x144 testsrc2, 30 fps, GOP 60, B-frames, 48 kHz AAC (91 KB).
- `tests/fixtures/av_opus.mp4`: same shape, Opus audio instead (48 kHz mono, ffmpeg `-c:a libopus`), the WHIP publisher's typical output.

## Decisions
- URIs: `s{msn}.m4s` (full segment = its parts back to back), `s{msn}.p{i}.m4s` (part), `init.mp4`.
- The primary track (video, or audio when there's no video) decides all cuts. A part closes as soon as the next frame would push it past `part_ms`, so no part exceeds PART-TARGET. It closes when the frame after its last one arrives. A segment cuts on the first keyframe at or after `segment_ms`. Audio goes into the part that is open when the audio sample starts.
- Every decode time is shifted by +10 s (exact in every timescale), so small negative RTMP timestamps stay valid in `tfdt`.
- Window: 6 full segments + the open one. Parts are listed for the last 3 segments (counting the open one).
- TARGETDURATION = max(ceil(segment_ms), rounded longest segment seen). It only grows. If the GOP is longer than `segment_ms`, it changes once, early in the stream.
- Blocking reload: `_HLS_msn`/`_HLS_part` wait until that part exists, or any later one does. `_HLS_msn` without a part waits for that full segment. Timeout 3 × TARGETDURATION → 503. 400 when the msn is more than 2 past the open segment, when the part is more than 3 past the newest part of the open segment, or when `_HLS_part` comes without `_HLS_msn`.
- The preload-hinted part blocks until it is complete (with the same timeout). Any other missing part or segment → 404 immediately.
- `init.mp4` is `no-cache`, because a track change replaces it at the same URL. Parts and segments are `max-age=60`.
- A track change with a different init clears the old segments (msn keeps counting). The next segment carries `EXT-X-DISCONTINUITY`.
- `Lagged` flushes the open part and segment, waits for a keyframe, and marks a discontinuity if the gap is over 500 ms or goes backwards. A jump of more than 5 s (or backwards) between consecutive frames is handled the same way.
- `/play/{name}` for an unknown but valid name returns the page with status 404. The page keeps retrying, so opening it before the publisher starts works.
- Latency readout: `Date.now() - hls.playingDate` (PROGRAM-DATE-TIME of the frame on screen), falling back to `hls.latency`. Safari native uses `getStartDate() + currentTime`. The number excludes encoder delay.
- Width/height in `tkhd`/`avc1` come from `VideoParams`, or 0 when the ingest doesn't set them. Decoders read the SPS anyway. No SPS parsing yet.

## Known gaps
- Only one video (H.264/H.265) + one audio (AAC or Opus) track. AV1 is ignored.
- No `EXT-X-SKIP`/delta playlists, no `EXT-X-RENDITION-REPORT` (single rendition).
- Republishing a name restarts msn at 0.

## Opus (batch 4, agent N, 18 Sep 2026)
- `Packager::set_tracks` now also accepts an audio track with `Codec::Opus` whose `init` parses as a valid `OpusHead` (magic, length, mapping family 0). `codec_string` reports it as `CODECS="opus"` (RFC 6381 / common practice: no profile suffix, unlike `mp4a.40.x`).
- Every audio frame is already a keyframe (`Lane::sample` forces it), so nothing changed there. What did change: `Sample.dur` for the *last* pending frame on a track — the one with no next frame to measure a gap against, at `flush()` (end of stream, a lag, a timestamp jump/discontinuity) — used to reuse the previous frame's measured duration (`last_dur`), which is right for AAC's fixed 1024-sample frames but wrong for Opus, whose frame size can change packet to packet (DTX, a config switch). `Lane::fallback_dur` now reads that last packet's own TOC byte via `opus::frame_duration_samples` instead, for Opus only. Every frame in the *middle* of a run still gets its duration from the measured `dts` delta between it and the next frame, which matches the TOC-declared duration whenever the sender's timestamps are honest (RTP/WebRTC Opus always advances the clock by the packet's own sample count).
- Verified with ffmpeg-generated H.264 + Opus, audio-only Opus (by construction: same code path, primary track becomes audio when there's no video), and H.264 + AAC (unchanged, still 19/19 unit tests green). Real check: `ffprobe`/`ffmpeg -f null -` against a live router serving a looped `av_opus.mp4` — no errors, both `h264` and `opus` listed.
- `mediastreamvalidator` flags `-50010: Unrecognized codec (opus)`: Apple's own HLS Authoring Spec never added Opus to its supported audio codec list (AAC-LC/HE-AAC/AC-3/EC-3/FLAC only), so this is Apple's validator saying "not an Apple codec," not a malformed stream — nothing in our fMP4 output is wrong. Matches the batch goal: Opus-in-HLS is for Chromium/Firefox via hls.js (MSE decodes Opus fine), not Safari/AVPlayer native playback. The other two MUST lines it reported (-50120 no HTTP/2, -50125 no rendition report) are pre-existing, already in the "Apple validator findings" section above, unrelated to Opus.

## e2e test issue (crates/caudal/tests/e2e.rs, not mine to edit)
The blocking check requests `_HLS_msn = MEDIA-SEQUENCE + count(EXTINF)`, `_HLS_part=0`. That is the *open* segment. Its part 0 usually already exists once the playlist shows parts, so a spec-correct server answers at once (RFC 8216bis §6.2.5.2), and the `>= 100 ms` assertion fails except by timing luck. Fix: request the preload-hinted part (parse `#EXT-X-PRELOAD-HINT` URI `s{m}.p{p}.m4s` → `_HLS_msn=m&_HLS_part=p`), or use `_HLS_part = <number of EXT-X-PART lines after the last EXTINF>`.


## Apple validator findings (orchestrator, 18 Sep 2026)
- Players enter through `master.m3u8` (multivariant, one variant with RFC 6381 `CODECS`, `RESOLUTION`, peak `BANDWIDTH`). The `/play` page loads it.
- `PART-HOLD-BACK` is 3 × part target + 1 ms; exactly 3× tripped -50102 through float rounding.
- A media playlist must not carry a rendition report about itself (-50099). With a single rendition, -50125 cannot be satisfied; see `KNOWN_MUST` in the e2e support.
- gohlslib (MediaMTX) writes no rendition reports and uses 2.5× hold-back, so it would fail the same checks.
