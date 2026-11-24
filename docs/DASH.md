# DASH output

Every stream the LL-HLS packager carries (H.264 / H.265 video, AAC / Opus
audio) is also served as live MPEG-DASH, for players that do not speak HLS:
dash.js-based web players, ExoPlayer / Media3 apps on Android and Android TV,
and smart TVs whose native player prefers DASH.

| URL | What |
|---|---|
| `/hls/{name}/manifest.mpd` | dynamic MPD (`type="dynamic"`), the entry point |
| `/hls/{name}/ll.mpd` | the low-latency (LL-DASH) MPD over the same files; see [Low-latency DASH](#low-latency-dash-llmpd) |
| `/hls/{name}/v-init{gen}.mp4`, `a-init{gen}.mp4` | one-track init segments (video, audio) |
| `/hls/{name}/v{msn}.m4s`, `a{msn}.m4s` | one-track media segments |
| `/dash/{name}/index.mpd` | the same manifest at MistServer's URL (its segment URLs resolve under `/dash/{name}/` too); `/dash/{name}/ll.mpd` is the LL one |
| `/dash/time` | the server clock (ISO 8601), for `UTCTiming` |

Segment and part length, window and reconnect grace come from `[hls]` (see
[HLS.md](HLS.md)); the only DASH keys are the LL manifest's service
description ([below](#low-latency-dash-llmpd)).

## Same segments, not a second packager

DASH reuses the LL-HLS packager's complete segments (`s{msn}.m4s`): same
cuts, same numbers, same window, same bytes in memory. Those segments carry
video and audio in one fragment, which DASH players do not accept (dash.js
refuses multiplexed Representations; ExoPlayer only exposes the type its
`AdaptationSet` declares). So the MPD has one `AdaptationSet` per media
type, and `v{msn}.m4s` / `a{msn}.m4s` are cut out of the stored segment on
each request: the `moof` keeps only that track's `traf`, the `mdat` only its
samples. Nothing is stored twice.

## What the MPD says

```xml
<MPD type="dynamic" availabilityStartTime="2026-11-21T15:00:00.000Z" publishTime="..."
     minimumUpdatePeriod="PT2.000S" timeShiftBufferDepth="PT12.000S"
     suggestedPresentationDelay="PT6.000S" minBufferTime="PT2.000S" ...>
  <Period id="p0" start="PT0.000S">
    <AdaptationSet id="0" contentType="video" mimeType="video/mp4" startWithSAP="1">
      <Representation id="cam" codecs="avc1.64001f" bandwidth="2400000" width="1280" height="720" frameRate="30">
        <SegmentTemplate timescale="90000" presentationTimeOffset="900000" startNumber="12"
                         initialization="../cam/v-init0.mp4" media="../cam/v$Number$.m4s">
          <SegmentTimeline><S t="1980000" d="180000"/>...</SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
    <AdaptationSet id="1" contentType="audio" mimeType="audio/mp4"> ... </AdaptationSet>
  </Period>
  <UTCTiming schemeIdUri="urn:mpeg:dash:utc:http-iso:2014" value="http://host:8080/dash/time"/>
  <UTCTiming schemeIdUri="urn:mpeg:dash:utc:direct:2014" value="..."/>
</MPD>
```

- **Timeline**: `SegmentTemplate` + `SegmentTimeline`, one `S` per complete
  segment in the window, with `t` = the segment's `tfdt` and `d` = the exact
  sum of its sample durations, per track (audio is cut on its own frame
  boundaries, so its `S` values are its own, not the video's). `$Number$`
  is the HLS media sequence number. Times are decode times; with B-frames
  the first picture's composition offset is not subtracted.
- **Codecs, size, rate**: the same RFC 6381 strings as `master.m3u8`;
  `width`/`height`/`frameRate` from the video track, `audioSamplingRate`
  and `AudioChannelConfiguration` from the audio track. `bandwidth` is the
  peak measured over the window's segments for that track.
- **Live timing**: `availabilityStartTime` is the wall clock of the
  stream's first segment; `timeShiftBufferDepth` is the window (6
  segments); `suggestedPresentationDelay` is 3 target durations (like HLS
  `HOLD-BACK`); `minimumUpdatePeriod` is one target duration, dropped once
  the stream has ended. `UTCTiming` names `/dash/time` (absolute, from the
  request's `Host`, `https` when `X-Forwarded-Proto: https`) and also
  carries the time directly.
- **Periods**: each discontinuity (a republish within
  `reconnect_grace_secs`, a new init segment, a timestamp jump) starts a
  new `Period`, `id="p{msn}"` of its first segment, with its own init
  segment and `presentationTimeOffset`. Its `start` is the wall clock at
  which its first segment arrived, never earlier than the end of the
  previous period, and is fixed once written: it never moves on later
  updates.
- **Renditions**: a family root (`/hls/cam/manifest.mpd`) adds every live
  `cam+*` rendition as another video `Representation` of the current
  period, each with its own numbers and timeline; renditions must share
  the root's timestamps, as `caudal-transcode`'s do. Audio comes from the
  root. A rendition's own manifest (`/hls/cam+720p/manifest.mpd`) lists
  only itself. A root whose own media is not fMP4-packaged (MPEG-2 from a
  broadcast feed) gives way to its renditions, as in `master.m3u8`.

## Low-latency DASH (`ll.mpd`)

`/hls/{name}/ll.mpd` is the same MPD in DASH-IF's low-latency mode
("Low-latency Modes for DASH", the DASH-IF IOP v4.3 change request,
clause 4.X.3; main live offering with `SegmentTimeline` and `$Number$`).
The LL-HLS packager already closes a part every `part_ms`; each part is
one `moof`+`mdat` pair, i.e. a CMAF chunk, so LL-DASH needs no new
packaging, only a different way of handing out the segment being built:

- **The open segment is listed.** As soon as the segment being built has
  its first part, the timeline gets an `S` for it with its real `t` and
  its nominal duration (`segment_ms`; a segment runs long when the GOP
  does, and the next update then lists its real `d`, like every complete
  segment's).
- **It is streamed.** A request for that segment (`v{msn}.m4s` /
  `a{msn}.m4s`) is answered at once with HTTP chunked transfer (no
  `Content-Length`): the parts it already has, cut to the one track as for
  complete segments, then each new part as it closes, and the body ends
  when the segment does. The chunks add up byte for byte to the complete
  segment, which later requests get whole, with a `Content-Length`, as
  before. A request for the segment after the open one waits for it to
  start, then streams it. Viewers share the packager's parts: each
  request holds at most one cut chunk in flight, nothing per viewer is
  stored. If no part arrives within three target durations (a publisher
  that stalled without closing the segment), the response is aborted
  rather than ended, so the player sees a failed request, not a short
  segment.
- **`SegmentTemplate@availabilityTimeOffset`** = `segment_ms` − `part_ms`
  in seconds (1.8 with the defaults) and
  **`@availabilityTimeComplete="false"`** (clause 4.X.3.2, 2.e/f: at most
  the segment duration minus its first chunk; the first part is never
  longer than `part_ms` and a segment never shorter than `segment_ms`).
  If `part_ms` ≥ `segment_ms` there is nothing to send early and `ll.mpd`
  is the plain MPD.
- **`ServiceDescription`** (clause 4.X.3.2; ISO/IEC 23009-1 Annex K):
  `Latency` `target`/`min`/`max` (ms, against `ProducerReferenceTime` 0)
  and `PlaybackRate` `min`/`max`.
- **`ProducerReferenceTime`** in every `Representation` (clause
  4.X.3.2, 1): `id="0" type="encoder"`, `presentationTime` = its
  `presentationTimeOffset` and `wallClockTime` = the period start (the
  wall clock at which the packager received the period's first frame),
  with a copy of the MPD's `UTCTiming`.
- `minimumUpdatePeriod` is `max(part_ms, 500 ms)`, so a player sees the
  next open segment soon after it starts; `minBufferTime` is one part.
  `suggestedPresentationDelay` stays the plain MPD's (3 target durations):
  LL players follow `Latency@target`, and players that ignore the LL
  attributes and wait for complete segments still get a safe delay.

```toml
[hls]
dash_target_latency_ms = 0     # 0 = segment_ms + 1000 (3000 with the defaults)
dash_min_latency_ms = 0        # 0 = half the target
dash_max_latency_ms = 0        # 0 = twice the target
dash_min_playback_rate = 0.95  # how much a player may slow down to rebuild buffer
dash_max_playback_rate = 1.05  # how much it may speed up to catch up
```

The automatic target is one second past a segment because DASH-IF asks for
the longest segment to be shorter than the target when an `AdaptationSet`
has several bitrates (a family of renditions). Inconsistent values are
repaired (`min` ≤ `target` ≤ `max`; rates outside `(0, 1]` / `[1, 100]`
fall back to the defaults).

Use `ll.mpd` with players in their low-latency mode (dash.js's
low-latency mode, ExoPlayer / Media3, which takes its live target from
`ServiceDescription`). `manifest.mpd` stays the plain MPD for everything
else, unchanged.

## Auth and sessions

Same as HLS: `?token=` or `Authorization: Bearer`, the same `[auth]` /
`[[access.rules]]` decision, and the token is carried onto the
`initialization` and `media` templates. A manifest request opens (or keeps
alive) the player's viewer session, counted under the `hls` output; a
kicked player gets 403. `/dash/time` needs no token.

## Not done

- LL-DASH is verified by Caudal's own tests (the MPD attributes; over a
  real socket, the open segment's first chunk arriving within
  milliseconds of the request while its body runs until the segment
  closes, and its chunks equal to the complete segment byte for byte, for
  several viewers at once), by GStreamer playing `ll.mpd` (as a plain
  client: its `dashdemux` ignores `availabilityTimeOffset` and fetches
  complete segments) and, in CI, by ffmpeg's DASH demuxer. Not yet with
  dash.js or ExoPlayer in low-latency mode, which are the players it is
  for; no `Resync` / in-segment random access signalling, no inband MPD
  validity events.
- Seen while testing: GStreamer's `dashdemux` (1.28) joining a live MPD
  whose window is already full skips a segment or two at its first
  manifest update (plain `manifest.mpd` and `ll.mpd` alike; not when it
  joins before the window fills). Not investigated yet.
- No WebVTT/captions `AdaptationSet`, no SCTE-35 `EventStream` yet.
- A segment with no samples of a track (audio that stopped mid-stream)
  keeps its `S` entry for numbering but its `a{msn}.m4s` answers 404.
- Verified with ffprobe/ffmpeg (each Representation's init + segments),
  ffmpeg's own DASH demuxer on the live MPD (`-xerror`, where ffmpeg is
  built with libxml2, as in CI) and GStreamer's `dashdemux` playing it in
  real time (see `crates/caudal-hls/tests/dash.rs`). Not yet with dash.js,
  ExoPlayer, a TV or the DASH-IF conformance validator.

## Compared with MistServer

MistServer serves DASH at `/dash/{name}/index.mpd`; Caudal answers that URL
too (same manifest, same auth and sessions), so a player configured for
MistServer keeps working. The canonical URL is `/hls/{name}/manifest.mpd`.
