# `[auth.hook]`: ask your own service before a publish or a play

Caudal can call an HTTP endpoint you run and wait for its answer before it
lets an encoder publish or a viewer play. Use it when the decision lives in
your system (accounts, payments, schedules, per-viewer limits) rather than in
a token Caudal can check by itself.

It is the equivalent of MistServer's blocking triggers (`USER_NEW`,
`STREAM_PUSH`), built around what goes wrong with those (see
`docs/research/MISTSERVER-REVERSE.md` §8): MistServer waits up to 5 s per
attempt and retries 5 times, so a handler that is down can hold a publish for
~25 s, and the documentation gives no number at all.

```toml
[auth.hook]
url = "https://auth.example.com/caudal"
secret = "whsec_..."        # Standard Webhooks secret, as in [hooks]
timeout_ms = 2000           # one attempt, no retries (100..=10000)
on_error = "deny"           # or "allow": what a timeout or a 5xx means
reply = "status"            # or "mistserver": how a 2xx is read (below)
publish = true              # ask before a publish
play = true                 # ask before a play
allow_cache_secs = 30       # remember an allow this long
deny_cache_secs = 5         # remember a denial or a failure this long
```

## Order

Every protocol goes through the same check, in this order. The first refusal
ends it. A stream key ([STREAM-KEYS.md](STREAM-KEYS.md)) is looked up first:
it names the stream, skips the rewrite and stands in for the token (step 3),
and the hook then gets no token. Otherwise, if `[auth.rewrite]` is set
([REWRITE-HOOK.md](REWRITE-HOOK.md)), it runs first and every step below
judges the name it returned:

1. Cluster node tokens (play only, inside a `[cluster]`).
2. `[[access.rules]]` (IP / CIDR / country).
3. `[auth]` tokens, when `publish` / `play` require them.
4. `[auth.hook]`.

So a request your network rules or tokens already refuse never reaches your
service, and your service can still refuse a request with a valid token.

## The request

`POST url`, `content-type: application/json`, signed per
[Standard Webhooks](https://www.standardwebhooks.com/) (`webhook-id`,
`webhook-timestamp`, `webhook-signature`). Verify the signature: anyone can
POST to your endpoint.

```json
{
  "type": "gate.publish",
  "timestamp": "2026-11-19T21:04:05Z",
  "data": { "stream": "cam", "ip": "203.0.113.9", "token": "..." }
}
```

`type` is `gate.publish` or `gate.play`. `ip` is `null` when the protocol
cannot tell. `token` is whatever the client presented (`?token=`,
`Authorization: Bearer`, SRT stream id), or `null`; it is passed through so
your service can check its own credentials. Use `https://` for anything off
the machine. Redirects are never followed, so the token only goes to `url`.

## The answer

`reply` says how a 2xx is read. The rest is the same in both modes.

| Your service answers | `reply = "status"` (default) | `reply = "mistserver"` |
|---|---|---|
| 2xx, body true (below) | allows | allows |
| 2xx, any other body (`false`, `0`, empty...) | allows | refuses |
| 401 or 403 | refuses; an optional `{"reason": "..."}` body is logged | same |
| anything else, a timeout, a connection error | `on_error` decides (`deny` by default) | same |
| 2xx whose body is over 4 KiB or not read within `timeout` | (body not read) | `on_error` decides |

### `reply = "mistserver"`

A MistServer blocking trigger handler (`USER_NEW`, `STREAM_PUSH`,
`CONN_PLAY`...) answers `200` with `true` or `false` in the body. Under the
default `status` mode every one of those answers is a 2xx, so a handler moved
over from MistServer that answers `200 false` would let everyone in. Set
`reply = "mistserver"` for such a handler.

The body is read exactly as MistServer reads it (`Util::stringToBool`,
`lib/bitfields.cpp:62-73` at the commit pinned in
[`research/MISTSERVER-REVERSE.md`](research/MISTSERVER-REVERSE.md), applied
to trigger answers at `lib/triggers.cpp:291-297`):

1. Drop every whitespace byte, wherever it is (space, tab, `\n`, `\v`, `\f`,
   `\r`).
2. Lowercase the first four bytes left.
3. True if they start with `1`, `yes`, `true` or `cont`. Anything else is
   false, including an empty body.

So `true`, `TRUE\n`, ` yes`, `1`, `10` and `continue` allow; `false`, `0`,
`no`, `ok` and an empty body refuse.

Two differences from MistServer, both on purpose:

- MistServer reads a body only from a 2xx (`lib/downloader.cpp:45-48`,
  `lib/triggers.cpp:78-87`); any other status, or no answer, makes it use the
  handler's `default`, which is `true` when unset (`lib/triggers.cpp:289-290`).
  Here those cases follow `on_error`, which fails closed unless you say
  otherwise. A MistServer `default` of `false` is `on_error = "deny"`.
- 401 and 403 still refuse, as in `status` mode.

The cache never mixes the two modes: the mode is part of every cache key, and
a reload starts with an empty cache anyway.

## Caching

LL-HLS asks for permission on every playlist and part request, several times
a second per viewer. Each decision is remembered per action, stream, IP and
token: allows for `allow_cache_secs`, denials and failures for
`deny_cache_secs` (shorter, so a service that comes back takes over quickly).
Identical requests that arrive while a call is in flight wait for that call
instead of sending their own. A revoked viewer can keep playing for up to
`allow_cache_secs`; lower it if that matters more than the load on your
service.

## Metrics

- `caudal_auth_hook_calls_total{action, outcome}`, outcome one of `allow`,
  `deny`, `error_allowed`, `error_denied`.
- `caudal_auth_hook_cached_total{action}`: requests answered without a call.
- `caudal_auth_hook_wait_seconds_total{action}`: time spent waiting on your
  service; divide by the calls to get the average.

## Reload

`[auth.hook]` reloads with `[auth]` (SIGHUP or `POST /api/v1/config/reload`).
A reload starts with an empty cache; the counters keep counting.
