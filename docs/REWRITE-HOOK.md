# `[auth.rewrite]`: let your service decide which stream a request goes to

Caudal can ask an HTTP endpoint you run which stream a publish or a play
really goes to, and wait for the answer. Use it to route: an encoder pushes
to `rtmp://host/live/customer-42-key` and lands on `event-2026`, a viewer
asks for `/hls/tonight/...` and is served `studio-b`, a key that is no
longer valid gets no stream at all.

It is the equivalent of MistServer's `PUSH_REWRITE` (an incoming push) and
`PLAY_REWRITE` (a viewer request); see
`docs/research/MISTSERVER-REVERSE.md` §1.4 and §4.2. It uses the same
machinery as [`[auth.hook]`](AUTH-HOOK.md): Standard Webhooks signing, one
attempt with a short timeout, `on_error`, a cache in which identical
requests share one call.

```toml
[auth.rewrite]
url = "https://route.example.com/caudal"
secret = "whsec_..."        # Standard Webhooks secret, as in [hooks]
timeout_ms = 2000           # one attempt, no retries (100..=10000)
on_error = "deny"           # or "allow": keep the requested name
publish = true              # route publishes (PUSH_REWRITE)
play = true                 # route plays (PLAY_REWRITE)
cache_secs = 30             # remember an answer this long at most
deny_cache_secs = 5         # remember a denial or a failure this long
```

It sits under `[auth]` and not `[hooks]` because it behaves like
`[auth.hook]`: it blocks the request, fails closed by default and reloads
with `[auth]`. `[hooks]` only notifies after the fact and never blocks.

## Order

1. `[auth.rewrite]` turns the requested name into the final name.
2. The final name must be a valid stream name (letters, digits, `_ - . +`,
   at most 128), or the request is refused.
3. Every usual check runs on the **final** name: kicks, cluster node
   tokens, `[[access.rules]]`, `[auth]` tokens, `[auth.hook]` (see
   [AUTH-HOOK.md](AUTH-HOOK.md#order)).

So a rewrite can never route a client around your access rules. It also
means that, with `[auth] play = true`, a viewer's token must name the
stream it is *served* (its `sub` is `real`, or a pattern such as `real*`),
not the alias it asked for. Since your service sees the token, it can also
pick the stream from it.

A play from another node of a `[cluster]` (a valid inter-node token) is not
routed again: the edge already asked for the final name.

## The request

`POST url`, `content-type: application/json`, signed per
[Standard Webhooks](https://www.standardwebhooks.com/) (`webhook-id`,
`webhook-timestamp`, `webhook-signature`). Verify the signature.

```json
{
  "type": "rewrite.publish",
  "timestamp": "2026-11-21T21:04:05Z",
  "data": { "stream": "alias", "protocol": "rtmp", "ip": "203.0.113.9", "token": "..." }
}
```

`type` is `rewrite.publish` or `rewrite.play`. `protocol` is one of `rtmp`,
`srt`, `whip`, `whep`, `rtsp`, `hls`, `moq`. `ip` is `null` when the
protocol cannot tell (MoQ). `token` is what the client presented (`?token=`,
`Authorization: Bearer`, SRT stream id), or `null`. MistServer also sends
the full request URL and host; Caudal does not, because not every protocol
has one to give. Redirects are never followed.

## The answer

| Your service answers | Caudal |
|---|---|
| 2xx, `{"stream": "real"}` | goes on under `real` (the same name keeps it) |
| 2xx, `{"stream": "real", "cache_secs": 0}` | the same, remembered at most `cache_secs` (0: not at all) |
| 2xx, plain text `real` (MistServer's trigger answer) | goes on under `real` (first line, trimmed) |
| 2xx with `stream` empty, null or missing, or an empty body | refuses (MistServer: "empty denies") |
| 401 or 403 | refuses; an optional `{"reason": "..."}` body is logged |
| anything else, a body over 4 KiB, a timeout, a connection error | `on_error`: `deny` refuses, `allow` keeps the requested name |

## Caching

Answers are remembered per action, protocol, requested name, client IP and
token, so different viewers asking for the same name can be sent to
different streams. LL-HLS asks for every playlist and part (several times a
second per viewer); with the cache, each viewer costs your service one call
per `cache_secs`. Return `cache_secs` to shorten it for an answer that
changes quickly; it can never make an answer live longer than
`cache_secs`. Denials and failures are remembered for `deny_cache_secs`.

## What each protocol does with the new name

| Protocol | Publish | Play | Notes |
|---|---|---|---|
| RTMP | yes | (no RTMP play) | the stream key is the requested name; `?token=` is passed |
| SRT | yes | yes | `publish/<name>`, `play/<name>`, `#!::r=<name>,m=...` |
| WHIP / WHEP | yes | yes | `Location` names the final stream, so `DELETE` works |
| RTSP | (no RTSP publish) | yes | routed at `DESCRIBE`; `SETUP` and `PLAY` keep the URL the client used |
| LL-HLS and TS-HLS | — | yes | playlists and segments stay under the requested name; the master's own variant keeps it too. Renditions `<root>+<name>` of a routed root are listed under their real names and are asked about like any other play |
| MoQ | yes | **refused if renamed** | a MoQ viewer subscribes by path inside the origin, and nothing there can serve `real` under the path `alias`; a play the hook renames is refused (404), one it keeps or denies works as expected |

Not routed: the control endpoints that name a stream without carrying
media (failover `switch`, channel `skip`, recording list/VOD/clips), and
pulls, pushes and transcodes configured on the server itself.

## `STREAM_SOURCE`: not done

MistServer's `STREAM_SOURCE` returns the source URL for a stream before its
input starts. Caudal has no per-request input start to hook: a stream exists
because a publisher connected or a configured pull is running. An on-demand
pull (a viewer asks for a name nobody publishes, the hook returns
`srt://...` or `https://.../index.m3u8`, Caudal starts that pull until the
last viewer leaves) would fit the core's `Demand` trait, but that slot is
the cluster edge's, and the pull lifecycle (stopping it, failures,
concurrent first viewers) is a feature of its own. Not built here.

## Metrics

- `caudal_rewrite_hook_calls_total{action, outcome}`, outcome one of
  `rewrite` (another name came back), `keep` (the same name), `deny`,
  `error_allowed`, `error_denied`.
- `caudal_rewrite_hook_cached_total{action}`: requests answered without a call.
- `caudal_rewrite_hook_wait_seconds_total{action}`: time spent waiting on
  your service.

## Reload

`[auth.rewrite]` reloads with `[auth]` (SIGHUP or
`POST /api/v1/config/reload`); removing it removes the routing. A reload
starts with an empty cache; the counters keep counting. A section that
cannot start refuses what it would have routed rather than letting it
through unrouted.
