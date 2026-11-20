# Viewer sessions

Who is watching what right now, and a way to end one viewer's connection.
Both endpoints are under `/api/`, so they need the admin login when
`[admin]` is configured.

## List

`GET /api/v1/sessions[?stream=NAME]`, oldest first:

```json
[
  {
    "id": 17,
    "stream": "cam",
    "protocol": "srt",
    "ip": "203.0.113.9",
    "started": 1763586245,
    "duration_secs": 312,
    "bytes_out": 48211904
  }
]
```

`bytes_out` is what the output wrote at its own layer: MPEG-TS for SRT, RTP
for RTSP, media payload before RTP/SRTP for WHEP.

**Covered today:** SRT, RTSP and WHEP viewers. **Not yet:** LL-HLS (plain
HTTP requests, no connection to attach a session to) and MoQ. Those viewers
are still counted in a stream's `viewers`.

## Kick

`DELETE /api/v1/sessions/{id}[?block_secs=N]` ends that viewer's connection
at once: SRT closes the socket, RTSP closes the control connection, WHEP
disconnects the peer.

With `block_secs` (at most 86400), the viewer's address may not play **that
stream** again for that long, over any protocol; other streams are not
affected. Without it, a player that reconnects by itself is back within
seconds. The block is checked before `[[access.rules]]`, `[auth]` tokens and
`[auth.hook]`, so a cached allow from the hook does not let it back in.

Answers: `200 {"kicked": 17, "blocked": true}`, `404` for an unknown id,
`400` for a block over a day. Blocks live in memory and end on restart.
