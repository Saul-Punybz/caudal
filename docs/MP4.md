# Progressive MP4 output

Every live stream is also served as one endless fragmented MP4 (fMP4)
response, the way MistServer serves `/{name}.mp4`:

| URL | Content-Type | Tracks |
|---|---|---|
| `/mp4/{name}.mp4` | `video/mp4` | the first H.264 or H.265 video and the first AAC or Opus audio |

A plain `<video src="http://host:8080/mp4/oceano.mp4" autoplay muted>`,
VLC, `ffmpeg -i http://host:8080/mp4/oceano.mp4 ...` or a download opens
it; no HLS or DASH player is involved. No config is needed; the route is
on the main HTTP (and HTTPS) port for every stream.

## Behaviour

- **Layout.** `ftyp` + `moov` once, then `moof` + `mdat` fragments: one
  per GOP, closed at each video keyframe (a GOP longer than 2 s is cut
  into 2 s fragments; an audio-only stream is cut every second). Audio
  and video are muxed into the same fragments. The boxes come from
  `caudal-cmaf`, the writer LL-HLS and recording use.
- **Live edge.** A viewer starts at the newest video keyframe; decode
  times count from that instant (the first fragment's `tfdt` is 0), so
  every viewer's file starts at 0 and plays from its first byte.
- **Timeline.** Decode times only grow. A source timestamp jump (backwards,
  or forward by more than 5 s) is stitched: the track continues where it
  was.
- **Slow clients.** At most 8 fragments wait for one connection; while
  they wait the viewer stops reading the stream, and the stream's shared
  buffer moves it forward to a later keyframe. What it missed is dropped,
  which leaves a gap in the timeline (decode times jump forward). Memory
  per viewer stays bounded. A client that takes nothing for 15 s is
  closed.
- **End.** The response ends (chunked body closes) when the publisher
  leaves, or when the stream's tracks change in a way one init segment
  cannot describe (new codec or new parameter sets). Players and
  `ffmpeg -reconnect 1` reconnect and get the new tracks.
- **One track.** `?audio=0` serves the video alone, `?video=0` the audio
  alone.
- **Codecs MP4 here does not carry.** MPEG-2 video (broadcast feeds), MP2,
  AC-3 and the rest answer `415`, naming `/ts/{name}.ts` (HTTP-TS carries
  MPEG-2 video and MPEG audio) and the `?video=0` / `?audio=0` that would
  serve the track that does fit. AV1 is not written into MP4 yet.

## Auth and sessions

Same rules as HLS and `/audio/`: a play token as `?token=...` or
`Authorization: Bearer ...` when `[auth]` or `[auth.hook]` require one
(`401` without, `403` refused), `[[access.rules]]` by the client address
(`[server] trusted_proxies` for `X-Forwarded-For`). A name nobody
publishes is `404`. On a cluster edge the first authorized request pulls
the stream from an origin. The route is public under `[admin]` (it checks
its own tokens).

Each open response is a viewer session with protocol `mp4` in
`GET /api/v1/sessions` (`bytes_out` counts what was handed to the
connection) and in the stream's viewer count; kicking it ends the
response.

## Verified

`crates/caudal-mp4/tests/mp4.rs` publishes H.264 + AAC and H.265 + AAC
from ffmpeg (demuxed by `caudal-ts`, as SRT ingest does) through the
router and writes the body to a file: ffprobe reads the codecs, 640x360
and per-stream decode times that start at 0 and strictly increase, and
`ffmpeg -xerror` decodes it with no error. Also checked: `?audio=0` /
`?video=0`, `415` for MPEG-2 + MP2, `404`, tokens, kick, and that a
client reading nothing while 120 s of video arrive gets well under half of
it.

By hand against the real binary (21 Nov 2026, macOS): ffmpeg publishing
H.264 + AAC over RTMP; `curl` got `200`, `video/mp4`, chunked; the session
showed as `mp4` with rising `bytes_out`; `ffmpeg -i .../mp4/cam.mp4 -c copy`
remuxed it; a `curl` left open ended with exit 0 when the publisher
stopped; all three files probed as H.264 640x360 + AAC and decoded with
`ffmpeg -xerror`, including the one cut mid-fragment by a curl timeout. No
end-of-body I/O error showed up. Not tried yet: a browser `<video>`, VLC,
Safari.

## Not done

WebM/Matroska (MistServer serves `.webm` too), AV1 and VP9 in MP4, a
DVR start (`?start=`), and Opus in a progressive MP4 has only the
caudal-cmaf unit coverage (no ffprobe test here: the test fixtures come
through MPEG-TS, which does not carry Opus in `caudal-ts`).
