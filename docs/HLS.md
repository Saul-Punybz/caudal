# HLS output

Every published stream is served as HLS twice, from the same `/hls/` route:

| Rendition | URL | Segments | For |
|---|---|---|---|
| LL-HLS (default) | `/hls/{name}/master.m3u8` (media playlist `index.m3u8`) | fMP4/CMAF, 200 ms parts | browsers (hls.js, Safari), iOS/tvOS, Android, current smart TVs |
| Classic HLS | `/hls/{name}/ts.m3u8` | MPEG-TS (`t{seq}.ts`) | older set-top boxes and smart TVs, IPTV middleware, MPEG-2 / MP2 feeds |

```toml
[hls]
part_ms = 200
segment_ms = 2000     # both renditions cut on the first keyframe past this
reconnect_grace_secs = 10
ts = true             # serve /hls/{name}/ts.m3u8 (false: 404)
```

## Classic HLS with TS segments

Some players only know the HLS of 2010: a version 3 playlist of MPEG-TS
segments, no fMP4, no partial segments. Older set-top boxes, smart TVs
from before about 2017 and most IPTV middleware are like that. TS can
also carry codecs fMP4 does not carry here: MPEG-2 video and MP2 audio
from an ATSC or DVB feed go out as they came in, so a broadcast channel
plays on those devices without a transcode.

### How a player finds it

By URL only. Point the device at `http://host:8080/hls/{name}/ts.m3u8`
(add `?token=...` when play auth is on). `master.m3u8` does not list it:
an HLS player that understands the multivariant playlist also understands
fMP4, and listing TS variants next to the LL-HLS ones would make hls.js
and Safari pick between two copies of the same video. LL-HLS URLs and
playlists are unchanged.

### What the playlist looks like

```
#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:13
#EXTINF:2.000,
t13.ts
#EXTINF:2.000,
t14.ts
...
```

- **Segments** are cut on video keyframes: a segment ends at the first
  keyframe at least `[hls] segment_ms` after it began, so a 2 s GOP gives
  2 s segments and a longer GOP gives longer ones. Audio-only streams cut
  on audio frames.
- **Each segment decodes on its own**: it starts with PAT and PMT, then
  the keyframe (with SPS/PPS for H.264, VPS/SPS/PPS for H.265, the
  sequence header for MPEG-2).
- **One muxer per stream** writes every segment, so continuity counters,
  PTS/DTS and PCR run on across segment boundaries; a player that plays
  them back to back sees one continuous transport stream. PIDs: video
  0x101, audio 0x102, SCTE-35 cues 0x103 (from the first cue on).
- **Codecs**: H.264, H.265, MPEG-2 video; AAC, MP2, MP3. Opus has no TS
  mapping: an Opus track (a WebRTC publisher) is left out, and an
  Opus-only stream has no TS rendition.
- **Window**: the last 6 segments are listed; 2 more stay fetchable for a
  player that read the playlist just before it moved.
  `EXT-X-TARGETDURATION` is the longest segment so far, rounded up, and
  never shrinks.
- **`EXT-X-DISCONTINUITY`** before the first segment after a publisher
  reconnect (within `reconnect_grace_secs`), a timestamp jump, a track
  change or a lag; `EXT-X-DISCONTINUITY-SEQUENCE` counts the ones that
  slid out of the window.
- **`EXT-X-ENDLIST`** once the stream has ended (the LL-HLS playlist ends
  at the same moment).

### On demand

The TS rendition costs nothing until a player asks for it. The first
request for `ts.m3u8` starts a segmenter for that stream at its latest
keyframe and waits (up to 3 target durations) for the first segment.
After 30 s with no request for the playlist or a segment, the segmenter
stops and its segments are freed. A player that comes back later gets
media sequence numbers that keep counting, after a discontinuity.
Memory per active stream: about 8 segments, i.e. 16 s at the stream's
bitrate with 2 s segments.

### Auth and sessions

Same as LL-HLS: the same `?token=` / `Authorization: Bearer`, the same
`[auth]` / `[[access.rules]]` decision, and the token is carried onto
every segment URI in the playlist. Each player is one viewer session,
counted under the `hls` output.

### Compared with MistServer

MistServer serves TS-HLS at `/hls/{name}/index.m3u8`; in Caudal that
path is the LL-HLS media playlist, so the TS one lives at `ts.m3u8`. A
player configured with a MistServer TS-HLS URL needs `index.m3u8`
changed to `ts.m3u8`. MistServer's `SAMPLE-AES` encryption is not
implemented.
