# Pulling streams in: SRT caller, MPEG-TS over UDP, MPEG-TS over HTTP, HLS

Besides receiving publishers (RTMP, SRT listener, WHIP, MoQ) and pulling
cameras (`[[rtsp.pull]]`), Caudal can go and get a stream four more ways.
All can be added from the API too (`POST /api/v1/pulls`, see
`docs/STREAM-API.md`).

## `[[srt.pull]]`: call a remote SRT listener

```toml
[[srt.pull]]
stream = "venue"
url = "srt://203.0.113.7:9000?streamid=play/main&passphrase=...&latency=200"
```

Caudal is the caller, the far end listens: the usual setup when the source
sits behind a firewall that only lets you reach out. What arrives (MPEG-TS,
H.264/H.265 + AAC) is published as `stream`, exactly like an SRT publisher
connecting in. A dropped connection ends the stream; Caudal reconnects
(1 s, doubling to 30 s; back to 1 s once data flowed) and publishes it again.
The URL takes the same `streamid`, `passphrase` and `latency` options as
`[[srt.push]]`.

## `[[udp.pull]]`: MPEG-TS over UDP, unicast or multicast

```toml
[[udp.pull]]
stream = "tv"
url = "udp://239.1.1.1:5000"      # join this group
interface = "10.0.0.2"            # optional: the NIC to join on

[[udp.pull]]
stream = "encoder"
url = "udp://:5000"               # unicast to this port (udp://0.0.0.0:5000)

[[udp.pull]]
stream = "headend"
url = "rtp://239.1.1.2:5004"      # TS inside RTP (RFC 2250)
```

This is how a lot of broadcast gear moves video inside a building: an
encoder, VLC or a playout box sends a transport stream to a port, often to
a multicast group so several receivers share one copy. `udp://@group:port`
(VLC's spelling) is accepted. IPv6 groups (`udp://[ff15::1]:5000`) work
too.

- There is no connection, so the stream is published when the first
  datagram arrives and **ends after 5 s without one**; the next datagram
  publishes it again.
- Several programs on one machine can listen to the same group
  (`SO_REUSEADDR`/`SO_REUSEPORT`). Two unicast pulls on one port are
  refused at config check: they would split the datagrams between them.
- The socket asks for a 4 MB receive buffer. On Linux the kernel caps it at
  `net.core.rmem_max` (about 208 kB by default); for high bitrates raise it
  (`sysctl -w net.core.rmem_max=8388608`) or expect drops under load.
- Codecs: what `caudal-ts` demuxes: H.264, H.265, AAC (ADTS), SCTE-35, and
  **MPEG-2 video and MPEG audio Layer II/III** (stream types 0x02, 0x03,
  0x04), which is what an ATSC 1.0 chain carries (an encoder feeding a
  multiplexer and an exciter).
- MPEG-2 video and MP2 pass through as they are: the TS outputs (SRT play,
  `[[multicast]]`, `[[udp.push]]`, `[[srt.push]]`) carry them unchanged.
  Browser outputs (LL-HLS, WebRTC, MoQ) and recording skip those tracks: no
  browser decodes MPEG-2.
- To watch such a feed in a browser, give it a `[[transcode.ladder]]`: the
  ffmpeg engine takes MPEG-2 + MP2/MP3 in and publishes `<stream>+<label>`
  as H.264 + AAC. For a source browsers do not play, a rendition as tall as
  the source is made too (720p -> `+720p`: the codec changes, not the
  size); interlaced frames (1080i) are deinterlaced (`bwdif`) first. The
  original keeps going out untouched on the TS outputs, and
  `/hls/<stream>/master.m3u8` (so `/play/<stream>` and the dashboard
  player) lists the renditions, since the MPEG-2 original is not a variant.

```toml
[[udp.pull]]
stream = "catv"
url = "udp://:5000"            # what VLC sends to the multiplexer

[[udp.push]]
stream = "catv"                # MPEG-2 + MP2, untouched, to the multiplexer
url = "udp://10.0.0.20:5000"

[[transcode.ladder]]
streams = ["catv"]             # and an H.264 copy to watch: /play/catv
[[transcode.ladder.rendition]]
label = "720p"
height = 720
video_kbps = 3000
```
- One video and one audio: the first of each that the PMT lists. A second
  audio PID (an SAP) is ignored, logged once.

## `[[http.pull]]`: MPEG-TS served over HTTP(S)

Some sources serve their transport stream as one endless HTTP response
instead of sending it: VLC with `#standard{access=http,mux=ts,dst=...}`,
ffmpeg with `-listen 1 http://...`, encoders and IRDs with an "HTTP TS"
output, and what MistServer calls an `http-ts://` input.

```toml
[[http.pull]]
stream = "web"
url = "http://10.0.0.5:8080/stream.ts"   # or https://
```

- Caudal sends a GET and demuxes the body with the same code as SRT and
  UDP, so the same codecs work (H.264, H.265, AAC, MPEG-2, MP2/MP3,
  SCTE-35).
- The stream is published when the first bytes arrive: a source that is
  down leaves no empty stream. 5 s without bytes, or the response ending,
  ends the stream; the pull reconnects with backoff (1 s, doubling to 30 s,
  back to 1 s once data flowed).
- An HLS playlist (`.m3u8`) is refused at config check: it is not a TS
  body. Pull it with `[[hls.pull]]`.
- From the API: `POST /api/v1/pulls` with an `http://` or `https://` URL
  whose path does not end in `.m3u8`.

## `[[hls.pull]]`: a remote HLS or LL-HLS stream

What MistServer's HLS input does: Caudal reads a playlist somebody else
serves (a CDN, an encoder's HLS output, another media server) and
publishes it as a stream, so every Caudal output can carry it.

```toml
[[hls.pull]]
stream = "tv"
url = "https://cdn.example/live/master.m3u8"   # a master or a media playlist
```

- A master playlist: the variant with the highest `BANDWIDTH` is pulled.
- Live: Caudal starts three segments from the newest, as players do, and
  reloads the media playlist every target duration (half of one when
  nothing was new, RFC 8216 §6.3.4). Each segment is fetched once, in
  media-sequence order, and demuxed with the same code as SRT, UDP and
  HTTP-TS (H.264, H.265, AAC, MPEG-2, MP2/MP3, SCTE-35).
- LL-HLS playlists are read like plain ones: whole segments only, no
  parts and no blocking reload, so the delay is that of plain HLS (a few
  target durations behind the source).
- A segment that fails to download is skipped and logged; segments that
  left the playlist before Caudal fetched them leave a gap.
- `EXT-X-DISCONTINUITY` (an ad splice, another encoder), and a media
  sequence that starts over (the source restarted), are spliced: the
  frames after it continue the timeline where the previous ones ended, so
  viewers see one monotonic stream. Codec parameters that change across a
  discontinuity (a new resolution) are not announced again.
- The stream is published on the first segment: a source that is down
  leaves no empty stream. It ends after three target durations (at least
  4 s) without a new segment; the pull then retries with backoff (1 s,
  doubling to 30 s, back to 1 s once segments flowed).
- `EXT-X-ENDLIST`: the playlist is played to its end at its own pace (at
  most two target durations ahead of real time, so a finished playlist is
  not dumped at once), then the stream ends. Caudal keeps checking the
  playlist and resumes only if it goes live again; it is not replayed.
- MPEG-TS segments only. fMP4/CMAF segments (`EXT-X-MAP`, what Apple's
  LL-HLS examples and Caudal's own HLS output use), encrypted segments
  (`EXT-X-KEY` with a method) and packed-audio segments (`.aac` with ID3)
  are refused with a log line, retried every 30 s.
- From the API: `POST /api/v1/pulls` with an `http://` or `https://` URL
  whose path ends in `.m3u8` adds an `[[hls.pull]]`. A playlist at a path
  without `.m3u8` can still be pulled by writing `[[hls.pull]]` by hand.

## Tested with

- SRT: `srt-live-transmit` listening (`srt://:PORT`), fed by ffmpeg; the
  pull publishes H.264 + AAC, ends with the source, and republishes when it
  comes back.
- UDP: ffmpeg sending `-f mpegts udp://127.0.0.1:PORT`, `-f rtp_mpegts
  rtp://...`, and to a multicast group with TTL 0; the idle end and the
  republish are checked.
- MPEG-2 720p59.94 + MP2 over UDP (ffmpeg `-c:v mpeg2video -c:a mp2`, the
  shape of a VLC-fed ATSC chain): both tracks are published. A remux of
  that feed through Caudal's TS muxer is read back by ffprobe as
  `mpeg2video` 1280x720 60000/1001 + `mp2` and decodes with `ffmpeg
  -xerror`. Not tested: a real ATSC multiplexer (Technalogix TP1000) on
  the receiving end.
- HTTP: ffmpeg serving `-f mpegts -listen 1 http://127.0.0.1:PORT/...`
  (what VLC's `access=http,mux=ts` serves): H.264 + AAC published, the
  stream ends when the source goes away and is republished when it comes
  back; an MPEG-2 + MP2 source publishes both tracks. Not tested: VLC
  itself, HTTPS, a source behind a redirect.
- HLS: ffmpeg writing a live HLS (`-f hls -hls_time 1 -hls_list_size 5`,
  TS segments) into a directory served over HTTP inside the test: H.264 +
  AAC published, frames keep arriving, the stream ends when ffmpeg is
  killed. A master playlist whose highest-bandwidth variant is live MPEG-2
  + MP2 (the lower one does not exist): both tracks published. Two
  finished encodes joined by `EXT-X-DISCONTINUITY` with `EXT-X-ENDLIST`:
  played paced, on one monotonic timeline, the stream ends and is not
  replayed. Not tested: a real CDN, HTTPS, LL-HLS from a real LL-HLS
  server (only the parser sees `EXT-X-PART`), H.265 segments, a master
  playlist with alternate audio renditions (`EXT-X-MEDIA`, not used: the
  variant's own audio is what is pulled).
