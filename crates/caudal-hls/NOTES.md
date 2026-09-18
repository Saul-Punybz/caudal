# caudal-hls notes (batch 1, agent C)

## Layout
- `src/lib.rs`: `router`, the publish listener, one packager task per stream, HTTP handlers, blocking waits (`watch` channel bumped when a part/segment/init appears).
- `src/packager.rs`: synchronous segmenter (frames in, parts/segments/playlist out). No clock inside; the task stamps `SystemTime::now()` on each frame.
- `src/fmp4.rs`: `ftyp`+`moov` (avc1/avcC, hvc1/hvcC, mp4a/esds) and `moof`+`mdat` via `mp4-atom` 0.15.
- `src/mp4demux.rs`: small progressive-MP4 demuxer used only by the tests and `examples/demo.rs`. Not compiled into the library.
- `tests/fixtures/av.mp4`: 4 s, 256x144 testsrc2, 30 fps, GOP 60, B-frames, 48 kHz AAC (91 KB).

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
- Only one video (H.264/H.265) + one AAC track. Opus/AV1 are ignored.
- No `EXT-X-SKIP`/delta playlists, no `EXT-X-RENDITION-REPORT` (single rendition).
- Republishing a name restarts msn at 0.

## e2e test issue (crates/caudal/tests/e2e.rs, not mine to edit)
The blocking check requests `_HLS_msn = MEDIA-SEQUENCE + count(EXTINF)`, `_HLS_part=0`. That is the *open* segment. Its part 0 usually already exists once the playlist shows parts, so a spec-correct server answers at once (RFC 8216bis §6.2.5.2), and the `>= 100 ms` assertion fails except by timing luck. Fix: request the preload-hinted part (parse `#EXT-X-PRELOAD-HINT` URI `s{m}.p{p}.m4s` → `_HLS_msn=m&_HLS_part=p`), or use `_HLS_part = <number of EXT-X-PART lines after the last EXTINF>`.


## Apple validator findings (orchestrator, 18 Sep 2026)
- Players enter through `master.m3u8` (multivariant, one variant with RFC 6381 `CODECS`, `RESOLUTION`, peak `BANDWIDTH`). The `/play` page loads it.
- `PART-HOLD-BACK` is 3 × part target + 1 ms; exactly 3× tripped -50102 through float rounding.
- A media playlist must not carry a rendition report about itself (-50099). With a single rendition, -50125 cannot be satisfied; see `KNOWN_MUST` in the e2e support.
- gohlslib (MediaMTX) writes no rendition reports and uses 2.5× hold-back, so it would fail the same checks.
