# RTMP

One listener (`[rtmp] bind`, port 1935 by default) does both directions on
`rtmp://host/<app>/<stream>`, where `<app>` is `[rtmp] app` (`live` by
default):

- **Publish**: an encoder (OBS, ffmpeg, a hardware encoder) sends a stream
  in. H.264 and AAC, plus Enhanced RTMP H.265 and AV1. See
  [OBS.md](OBS.md).
- **Play**: a viewer reads a stream out: VLC, ffplay, OBS's media source,
  older players, and other servers pulling over RTMP (another Caudal's
  `[[rtmp.pull]]`, nginx-rtmp, Wowza, MistServer).

Pulling *from* a remote RTMP server is `[[rtmp.pull]]`, in
[PULL-INPUTS.md](PULL-INPUTS.md). Pushing *to* one is `[[restream]]`.

```toml
[rtmp]
bind = "0.0.0.0:1935"
app = "live"
play = true   # answer `play`; false = publish only
```

Changing `bind`, `app` or `play` restarts the listener on reload.

## Play

```
ffplay rtmp://caudal.example:1935/live/cam
ffmpeg -i rtmp://caudal.example:1935/live/cam -c copy cam.flv
vlc rtmp://caudal.example:1935/live/cam
```

- **Auth**: the same `[auth]` check as every other output (`Access::Play`).
  The token goes in the stream key's query, as for publishing:
  `rtmp://host/live/cam?token=<jwt>`. A refused viewer gets
  `onStatus` `NetStream.Play.Failed` (level `error`) and the connection
  closes. A viewer kicked with a block is refused the same way until the
  block expires.
- **Unknown stream**: `NetStream.Play.StreamNotFound`, then the connection
  closes. On a cluster edge the first viewer starts the pull from the
  origin, as with HLS or SRT, and waits for it.
- **What the viewer gets**: `StreamBegin`, `NetStream.Play.Reset`,
  `NetStream.Play.Start`, then `onMetaData` (codec ids, width, height,
  framerate, sample rate, channels), the sequence headers, and the stream
  from its newest keyframe on. A track change (the encoder restarted with
  other settings) sends `onMetaData` and the sequence headers again.
- **Codecs**: H.264 and AAC as classic FLV; H.265, AV1 and Opus as Enhanced
  RTMP (FourCC `hvc1`, `av01`, `Opus`), which needs a player that reads
  Enhanced RTMP (FFmpeg 6.1 or later). One video and one
  audio track are carried, the first of each kind RTMP can encode; other
  tracks (VP8, MP3, captions) are left out.
- **End of stream**: `StreamEOF` and `NetStream.Play.Stop`, then the
  connection closes; FFmpeg finishes its file and exits 0.
- **Slow viewers**: nothing is buffered per viewer beyond the message being
  written. A viewer that falls behind the live buffer (`[buffer]`) is moved
  to the next keyframe; one whose socket accepts nothing for 10 s is
  disconnected.
- **Sessions**: each viewer is a session with protocol `rtmp` in
  `GET /api/v1/sessions` (see [SESSIONS.md](SESSIONS.md)), counted in the
  stream's viewers, and can be kicked.
- Not supported: `play2`, seeking, pause, `receiveAudio`/`receiveVideo`
  (ignored), more than one `play` per connection, SCTE-35 cues out as
  `onCuePoint`, RTMPS on the listener.
