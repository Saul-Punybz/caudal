# DASH output

Every stream the LL-HLS packager carries (H.264 / H.265 video, AAC / Opus
audio) is also served as live MPEG-DASH, for players that do not speak HLS:
dash.js-based web players, ExoPlayer / Media3 apps on Android and Android TV,
and smart TVs whose native player prefers DASH.

| URL | What |
|---|---|
| `/hls/{name}/manifest.mpd` | dynamic MPD (`type="dynamic"`), the entry point |
| `/hls/{name}/v-init{gen}.mp4`, `a-init{gen}.mp4` | one-track init segments (video, audio) |
| `/hls/{name}/v{msn}.m4s`, `a{msn}.m4s` | one-track media segments |
| `/dash/{name}/index.mpd` | the same manifest at MistServer's URL (its segment URLs resolve under `/dash/{name}/` too) |
| `/dash/time` | the server clock (ISO 8601), for `UTCTiming` |

There is no configuration: segment length, window and reconnect grace come
from `[hls]` (see [HLS.md](HLS.md)).

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

## Auth and sessions

Same as HLS: `?token=` or `Authorization: Bearer`, the same `[auth]` /
`[[access.rules]]` decision, and the token is carried onto the
`initialization` and `media` templates. A manifest request opens (or keeps
alive) the player's viewer session, counted under the `hls` output; a
kicked player gets 403. `/dash/time` needs no token.

## Not done

- **No LL-DASH**: segments are listed once complete, there is no
  `availabilityTimeOffset` and no chunked transfer of the open segment's
  parts. Latency is that of plain DASH (about 3 segment durations), not
  LL-HLS's.
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
