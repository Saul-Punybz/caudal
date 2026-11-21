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
publish = true              # ask before a publish
play = true                 # ask before a play
allow_cache_secs = 30       # remember an allow this long
deny_cache_secs = 5         # remember a denial or a failure this long
```

## Order

Every protocol goes through the same check, in this order. The first refusal
ends it. If `[auth.rewrite]` is set ([REWRITE-HOOK.md](REWRITE-HOOK.md)), it
runs first and every step below judges the name it returned:

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

| Your service answers | Caudal |
|---|---|
| any 2xx | allows |
| 401 or 403 | refuses; an optional `{"reason": "..."}` body is logged |
| anything else, a timeout, a connection error | `on_error` decides (`deny` by default) |

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
