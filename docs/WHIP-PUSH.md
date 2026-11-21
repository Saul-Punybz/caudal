# WHIP push

Caudal takes WHIP publishers (`POST /whip/{name}`) and serves WHEP viewers.
It can also be the WHIP *client*: while a stream is live, publish it to a
remote WHIP endpoint (Cloudflare Stream, Dolby.io/Millicast, LiveKit's WHIP
ingress, another Caudal). This is MistServer's `whip://` push.

```toml
# Cloudflare Stream: the key is in the URL path.
[[webrtc.push]]
stream = "main"
url = "whip://customer-abc.cloudflarestream.com/<key>/webRTC/publish"

# Dolby.io / Millicast, LiveKit and most others: a bearer token.
[[webrtc.push]]
stream = "main"
url = "https://director.millicast.com/api/whip/main"
token = "<publish token>"

# Another Caudal on the LAN (plain HTTP), into its stream "relay".
[[webrtc.push]]
stream = "cam1"
url = "whip+http://10.0.0.7:8080/whip/relay"
token = "<that server's publish token>"
```

## URLs

| Scheme | Means |
|---|---|
| `whip://host[:port]/path` | WHIP over HTTPS (`https://host/path`). |
| `whip+http://host[:port]/path` | WHIP over plain HTTP. For a LAN or a local test; the SDP and the token travel in the clear. |
| `https://...`, `http://...` | Taken as they are, in `[[webrtc.push]]` only. In `[[push.auto]]` and `POST /api/v1/pushes` an `https://` URL could be anything, so there it must be `whip://` or `whip+http://`. |

`token`, when set, is sent as `Authorization: Bearer <token>` on the offer
`POST` and on the final `DELETE`. It must be printable ASCII without spaces.

## What happens

1. The push waits for its stream to be live and to announce its tracks.
2. It offers send-only H.264 (packetization-mode 1) and/or Opus, whichever
   the stream has, with one host candidate: the local address that routes to
   the endpoint. It `POST`s the offer as `application/sdp`.
3. It applies the answer (`201`, or any `2xx`) and its `a=candidate` lines,
   and keeps `Location`. There is no trickle ICE (`PATCH`): Caudal is a full
   ICE agent and finds the path with connectivity checks; behind NAT the
   remote learns its public address from those checks (peer-reflexive), as
   with any WHIP encoder that has no STUN server.
4. Once ICE and DTLS are up it starts at the newest keyframe, sends SPS/PPS
   before every IDR (from the avcC when the frames do not carry them) and
   answers PLI/FIR by skipping to the newest keyframe.
5. When the stream ends, the push sends `DELETE` to `Location` and waits for
   the next publish. A push removed by a reload also sends its `DELETE`.

A refused offer, an unreachable endpoint, or ICE/DTLS not coming up within
15 s is retried while the stream is live, waiting 1 s, 2 s, 4 s... up to
30 s; a session that did connect and then dropped reconnects after 1 s.

## Codecs

The same as WHEP: H.264 and Opus. A stream with AAC (or MP3, AC-3) audio is
pushed as video only, logged once; there is no audio transcoding on this
path. HEVC or AV1 video is not sent; a stream with neither H.264 nor Opus is
not pushed (logged and retried). B-frames are sent as they are, with a
warning, as for WHEP.

## Reloads and the API

`[[webrtc.push]]` is hot, unlike the rest of `[webrtc]`: a reload starts new
or changed pushes, stops removed ones (with their `DELETE`) and leaves an
unchanged push (same stream, URL and token) and its session alone.

- `GET /api/v1/pushes` lists them with `"section": "[[webrtc.push]]"`. The
  token is never listed; in the URL, user info, every query value and every
  path segment of 16 characters or more (a stream key) show as `****`. Logs
  mask the URL the same way.
- `POST /api/v1/pushes` with `{"stream": "main", "url": "whip://h/whip/x",
  "token": "..."}` adds one (`token` is optional and only accepted for WHIP).
- `DELETE /api/v1/pushes/{stream}` removes them, as for the other pushes.

## Automatic pushes

`[[push.auto]]` takes `whip://` and `whip+http://` templates, with an
optional `token` for every push the rule starts:

```toml
[[push.auto]]
streams = ["cam*"]
url = "whip://ingest.example/whip/$stream"
token = "<publish token>"
```

## Verified

Caudal pushing to Caudal: one registry's push into a second Caudal's WHIP
router over real UDP on localhost (`crates/caudal-webrtc/tests/whip_push.rs`):
the second server receives H.264 and Opus and frames keep arriving, the
bearer token reaches it, ending the source sends `DELETE`, removing the push
sends `DELETE`, and a failing endpoint is retried with a growing wait. Not
yet run against Cloudflare Stream, Millicast or LiveKit.
