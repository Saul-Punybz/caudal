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
    "bytes_out": 48211904,
    "rtt_ms": 18.4,
    "loss": 0.003,
    "tags": ["vip"]
  }
]
```

| Protocol | A session is | `ip` | `bytes_out` | `rtt_ms` / `loss` |
|---|---|---|---|---|
| SRT | one connection | yes | MPEG-TS sent | from SRT's own measurements; `loss` = share of packets retransmitted or dropped late |
| RTSP | one PLAY | yes | RTP sent | `null` (not measured yet) |
| WHEP | one peer | yes | media payload, before RTP/SRTP | from the peer stats: RTT, and the loss the viewer reports over RTCP |
| LL-HLS | one player: address + user agent, ended after 10 s without a playlist request | yes | every file it fetched | `null` |
| MoQ | one QUIC session; `stream` is `*` when it connected to the root and may see every stream | no (the MoQ library does not expose it) | 0 (stats are per broadcast, not per session) | `null` |

`null` means not measured, never zero.

## Tags

`POST /api/v1/sessions/{id}/tags` with `{"tag": "vip"}` labels a session
(1-64 characters of `A-Z a-z 0-9 . _ : -`, at most 16 per session):
`204`, `400` for a bad tag, `404` for an unknown session.

## Kick

`DELETE /api/v1/sessions/{id}[?block_secs=N]` ends one viewer:
SRT closes the socket, RTSP closes the control connection, WHEP disconnects
the peer, MoQ closes the QUIC session, and an LL-HLS player gets `403` on
every request until it has been quiet for 10 s.

`DELETE /api/v1/sessions?stream=NAME&tag=T[&block_secs=N]` ends every
session matching **all** the conditions given (at least one): a whole
stream's audience, everyone with a tag, or a tag within one stream. Answer:
`{"kicked": [17, 18]}`.

With `block_secs` (at most 86400), the viewer's address may not play **that
stream** again for that long, over any protocol; other streams are not
affected. Without it, a player that reconnects by itself is back within
seconds (LL-HLS: once it pauses for 10 s). The block is checked before
`[[access.rules]]`, `[auth]` tokens and `[auth.hook]`, so a cached allow from
the hook does not let it back in. MoQ sessions have no address, so they
cannot be blocked.

Answers: `200 {"kicked": 17, "blocked": true}`, `404` for an unknown id,
`400` for a block over a day. Blocks and tags live in memory and end on
restart.
