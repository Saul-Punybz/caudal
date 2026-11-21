# Pulling streams in: SRT caller and MPEG-TS over UDP

Besides receiving publishers (RTMP, SRT listener, WHIP, MoQ) and pulling
cameras (`[[rtsp.pull]]`), Caudal can go and get a stream two more ways.
Both can be added from the API too (`POST /api/v1/pulls`, see
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
  `[[multicast]]`, `[[srt.push]]`) carry them unchanged. Browser outputs
  (LL-HLS, WebRTC, MoQ), recording and the transcoder skip those tracks: no
  browser decodes MPEG-2, so watching such a feed in a browser needs a
  transcode to H.264 (not built yet).
- One video and one audio: the first of each that the PMT lists. A second
  audio PID (an SAP) is ignored, logged once.

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
