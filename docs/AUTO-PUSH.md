# Automatic pushes

A static push (`[[restream]]`, `[[srt.push]]`, `[[udp.push]]`,
`[[multicast]]`) names one stream. An automatic push is a rule: stream
patterns plus a target URL template. Whenever a stream matching the rule goes
live, Caudal starts a push for it; when the stream ends, the push stops. This
is MistServer's "automatic push" (`push_auto_add`).

```toml
# Every camera, and the main program, to an SRT receiver; one stream id each.
[[push.auto]]
streams = ["cam*", "main"]
url = "srt://203.0.113.4:9000?streamid=$stream"

# Every stream to a backup RTMP server, under its own name.
[[push.auto]]
streams = ["*"]
url = "rtmp://backup.example/live/$stream"

# The program feed to a multiplexer's IP input, as RTP/MP2T.
[[push.auto]]
streams = ["program"]
url = "rtp://10.0.0.20:5004"
ttl = 4                 # udp:// and rtp:// only, as in [[udp.push]]
interface = "10.0.0.5"  # udp:// and rtp:// only
pacing = true           # udp:// and rtp:// only, default true
```

## Rules

| | |
|---|---|
| `streams` | Exact names (`main`), `prefix*` (`cam*`), or `*`. The same matching `[transcode]` uses: a `name+rendition` stream (`main+720p`) never matches, so a rule pushes sources, not their renditions. |
| `url` | `rtmp://` / `rtmps://` (the `[[restream]]` engine), `srt://` (the `[[srt.push]]` engine), `udp://` / `rtp://` (the `[[udp.push]]` sender). No new protocol: each match runs on the same engine as the static entry of that scheme, with the same reconnect and backoff. |
| Variables | `$stream` or `${stream}`: the stream's name. `$basename` or `${basename}`: the part before `+`, which is the whole name here because `+` streams never match. Any other `$word` is left as it is (a stream key may contain `$`). MistServer's time variables (`$datetime`...) are not supported. |
| UDP/RTP | The URL is an IP address and port, so it takes no variables, and a destination carries one stream at a time. When several live streams match one UDP rule, the stream already sending keeps the destination (otherwise the first by name); the rest are listed as `skipped`. |

`caudal check` (and every reload) refuses a rule with an empty `streams`, a
pattern that is not a name, `prefix*` or `*`, an unknown scheme, a template
that does not expand to a URL with a host (and, for SRT, a port) for a sample
name, `ttl`/`interface`/`pacing` on a non-UDP rule, or a UDP destination that
a `[[multicast]]`, `[[udp.push]]` or another rule already uses.

## Duplicates

A target is a stream plus a URL. A static entry and a rule that produce the
same target run one push (the static one). Two rules that produce the same
target also run one. For UDP/RTP the key is the destination address, as above.

## Reloads

`[[push.auto]]` is hot. A reload (or a `POST`/`DELETE` on `/api/v1/pushes`)
recomputes every engine's target list from the static entries and the live
matches. A push whose stream and URL are unchanged keeps its task and its
connection; only added, removed or changed ones start or stop.

## Seeing it

- `GET /api/v1/auto-pushes`: each rule with what it runs now:

  ```json
  [{"streams": ["cam*", "main"], "url": "srt://203.0.113.4:9000?streamid=****",
    "kind": "srt", "active": [{"stream": "cam1", "url": "srt://203.0.113.4:9000?streamid=****"}],
    "skipped": []}]
  ```

- `GET /api/v1/pushes` keeps its shape (`[{"stream", "url", "section"}]`) and
  adds one row per rule pattern with `"section": "[[push.auto]]"` and the
  template as `url`. Those rows are read-only there: `DELETE
  /api/v1/pushes/{stream}` removes static pushes only; edit the file for a
  rule.
- The running pushes appear where their engine reports: RTMP in
  `GET /api/v1/restreams`, UDP/RTP in `GET /api/v1/multicast` and
  `caudal_multicast_*`.

Secrets are masked in every listing as for static pushes: the last path
segment of an RTMP URL (the stream key), SRT `passphrase` and `streamid`.

## Not yet

MistServer's schedule (`scheduletime`/`completetime`), start/end conditions
(`start_rule`/`end_rule`) and `inhibit` are not implemented; a rule pushes
for as long as the stream is live. Adding rules from the API is not either.
