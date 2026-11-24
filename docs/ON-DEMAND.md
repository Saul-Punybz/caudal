# On-demand pulls and `[auth.source]`

A pull normally runs all the time: Caudal connects at startup and keeps
reconnecting. An **on-demand** pull runs only while someone watches. The
first viewer who asks for the stream starts it. Players wait a few seconds
for the first media. The pull stops `idle_secs` after the last viewer left.
This is MistServer's on-demand input (see
`docs/research/MISTSERVER-REVERSE.md` §1.4). `[auth.source]` adds its
`STREAM_SOURCE` trigger: your service names the source of a stream Caudal
knows nothing about.

## A configured on-demand pull

Add `on_demand = true` to any `[[srt.pull]]`, `[[udp.pull]]`,
`[[http.pull]]`, `[[hls.pull]]`, `[[rtmp.pull]]` or `[[rtsp.pull]]`:

```toml
[[rtsp.pull]]
stream = "lobby"
url = "rtsp://admin:pass@10.0.0.5/stream1"
on_demand = true
idle_secs = 30        # optional, 1..=3600, default 30 (MistServer's)
```

Entries without `on_demand` keep running all the time, as before.
`[[omt.pull]]` has no on-demand mode.

## `[auth.source]`: your service names the source

```toml
[auth.source]
url = "https://sources.example.com/caudal"
secret = "whsec_..."        # Standard Webhooks secret, as in [hooks]
timeout_ms = 2000           # one attempt, no retries (100..=10000)
cache_secs = 10             # remember a source this long at most
deny_cache_secs = 5         # remember "no such stream" or a failure this long
idle_secs = 30              # stop a pull it started this long after the last viewer
```

Caudal asks your service when a viewer requests a stream that is not live
and has no pull entry. It sends `POST url` with a signed body
([Standard Webhooks](https://www.standardwebhooks.com/), like
[`[auth.rewrite]`](REWRITE-HOOK.md)):

```json
{ "type": "stream.source", "timestamp": "2026-11-21T21:04:05Z", "data": { "stream": "cam42" } }
```

| Your service answers | Caudal |
|---|---|
| 2xx, `{"source": "srt://10.0.0.9:9000?streamid=play/cam42"}` | pulls that on demand |
| 2xx, `{"source": "...", "cache_secs": 0}` | the same; the answer is kept at most `cache_secs` (0: not kept) |
| 2xx, plain text `rtsp://...` (MistServer's trigger answer) | pulls it (first line, trimmed) |
| 2xx with `source` empty, null or missing, or an empty body | no such stream: the viewer gets the output's 404 |
| 401, 403, anything else, a body over 4 KiB, a timeout | no such stream; logged and counted as an error |

There is no `on_error = "allow"`. When the service fails, Caudal has no URL
to start.

Caudal picks the pull from the URL's scheme. `srt://` gives an SRT caller,
`udp://` and `rtp://` a UDP listener, and `rtmp(s)://` an RTMP play. An
`rtsp(s)://` URL gives an RTSP camera pull. An `http(s)://` URL whose path
ends in `.m3u8` is pulled as HLS; any other `http(s)://` URL is read as
MPEG-TS over HTTP. Caudal refuses a URL of any other kind and logs it. The
URL goes through the same checks as a configured pull's.

Answers are cached by stream name only, unlike `[auth.rewrite]`. The source
of a stream does not depend on which viewer asks first: every viewer has
already passed `[auth]`, and they all share the one pull.

Like `[auth.hook]` and `[auth.rewrite]`, the section sits under `[auth]`.
It blocks the request, uses the same signing, timeout and cache code, and
reloads with `[auth]`.

## What happens when a viewer asks

1. Every output (LL-HLS and TS-HLS, WHEP, SRT, RTSP, RTMP play, MoQ, the
   `/ts/`, `/mp4/` and audio endpoints) handles the request as usual.
   `[auth.rewrite]`, the access rules, `[auth]` tokens and `[auth.hook]`
   all run first. An unauthorized request never starts a pull.
2. If the stream is live, it is served. If not, the output asks the
   registry for it (`Registry::get_or_demand`). The registry asks its
   demand sources in turn: first the on-demand pulls (a configured entry,
   else `[auth.source]`), then a `[cluster]` edge's origins.
3. Caudal starts the pull with the same code an always-on entry runs. It
   waits up to **10 s** for the first frame, then serves the stream. A
   name gets one pull no matter how many viewers arrive together. The
   others wait for it.
4. If no frame arrives within 10 s, Caudal stops the pull. Every waiting
   viewer gets the output's usual "not found" (HTTP 404, an SRT or RTSP
   refusal). For the next **5 s**, requests for that name get the same
   answer at once, without starting anything.
5. Four times a second, a task checks the stream's viewer count. That count
   includes subscribers and HLS, MoQ and OMT players; recorders and
   packagers do not count. When the count has been zero for `idle_secs`,
   the task stops the pull and the stream ends. The first viewer also gets
   `idle_secs` to show up in the count. HLS players count as watching
   until 10 s after their last playlist request, so an HLS-only stream
   stops about `idle_secs` + 10 s after the last player.

If the source drops while viewers watch, the pull reconnects as an
always-on pull does. When a reload removes or changes a configured entry,
Caudal stops that entry's pull now. Players reconnect and start the new
one. A pull that `[auth.source]` started keeps running until it goes idle,
even if the section is removed.

## API and metrics

`GET /api/v1/pulls` (see [STREAM-API.md](STREAM-API.md)) lists each
configured pull with these fields:

- `on_demand`
- `state`: `idle`, `starting`, `live`, `stopping` or `failed` for an
  on-demand pull; `live` or `connecting` for an always-on one
- `idle_secs`
- `viewers`

While a pull that `[auth.source]` started is running, the list shows it
with `"section": "[auth.source]"`.

Metrics:

- `caudal_on_demand_pulls{state}` (gauge): on-demand pulls in `starting`,
  `live` or `stopping`.
- `caudal_on_demand_starts_total`, `caudal_on_demand_failures_total` (no
  media in time), `caudal_on_demand_idle_stops_total`.
- `caudal_source_hook_calls_total{outcome}`, where `outcome` is `source`,
  `none` or `error`; `caudal_source_hook_cached_total`;
  `caudal_source_hook_wait_seconds_total`.

## Differences from MistServer

- MistServer starts an input process per stream. Caudal starts a task in
  its own process.
- MistServer's `STREAM_SOURCE` can also override the source of a
  configured stream. In Caudal, a configured entry always wins, and the
  hook is asked only for names that nothing configures.
- The wait for the first media (10 s) and the backoff (5 s) are fixed.
