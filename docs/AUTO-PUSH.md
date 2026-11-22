# Automatic pushes

A static push (`[[restream]]`, `[[srt.push]]`, `[[udp.push]]`,
`[[multicast]]`, `[[webrtc.push]]`) names one stream. An automatic push is a rule: stream
patterns plus a target URL template. Whenever a stream matching the rule goes
live, Caudal starts a push for it; when the stream ends, the push stops. This
is MistServer's "automatic push" (`push_auto_add`). A rule can also be limited
to a time window and to conditions ([below](#schedules-and-conditions)).

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
| `url` | `rtmp://` / `rtmps://` (the `[[restream]]` engine), `srt://` (the `[[srt.push]]` engine), `udp://` / `rtp://` (the `[[udp.push]]` sender), `whip://` / `whip+http://` (the `[[webrtc.push]]` WHIP client; an optional `token` is the bearer token for every push of the rule, see `docs/WHIP-PUSH.md`). No new protocol: each match runs on the same engine as the static entry of that scheme, with the same reconnect and backoff. |
| Variables | `$stream` or `${stream}`: the stream's name. `$basename` or `${basename}`: the part before `+`, which is the whole name here because `+` streams never match. Any other `$word` is left as it is (a stream key may contain `$`). MistServer's time variables (`$datetime`...) are not supported. |
| UDP/RTP | The URL is an IP address and port, so it takes no variables, and a destination carries one stream at a time. When several live streams match one UDP rule, the stream already sending keeps the destination (otherwise the first by name); the rest are listed as `skipped`. |

## Schedules and conditions

A rule may also carry a time window and conditions. Each is a gate: while
any gate is closed the rule pushes nothing, and when they all open again
the push starts by itself (if a matching stream is live). Gates are checked
in this order, and `GET /api/v1/auto-pushes` names the first one that is
closed.

```toml
# Christmas Eve special: only from 20:00 to 23:00 Puerto Rico time.
[[push.auto]]
streams = ["main"]
url = "rtmp://a.rtmp.youtube.com/live2/xxxx-xxxx"
start = "2026-12-24T20:00:00-04:00"
stop  = "2026-12-24T23:00:00-04:00"

# Weekday evening show, one hour from 18:00 local time.
[[push.auto]]
streams = ["show"]
url = "srt://203.0.113.4:9000?streamid=$stream"
cron = "0 18 * * MON-FRI"
tz = "America/Puerto_Rico"
duration_secs = 3600

# The backup feed goes out only while the main feed is down.
[[push.auto]]
streams = ["backup"]
url = "rtmp://cdn.example/live/program"
inhibit = ["main"]

# Kept in the file, not pushing; the API can turn it on.
[[push.auto]]
streams = ["cam*"]
url = "srt://198.51.100.7:9000?streamid=$stream"
enabled = false
```

| Field | |
|---|---|
| `enabled` | Default `true`. `false` keeps the rule but pushes nothing. `POST /api/v1/auto-pushes/{index}/enable` or `/disable` overrides it at runtime without editing the file (see below). |
| `start`, `stop` | A one-off window: RFC 3339 instants with an offset (`Z` or `-04:00`). Either may be left out: only `start` pushes from then on, only `stop` pushes until then. `start` plus `duration_secs` is the same as `start` plus `stop`. MistServer's `scheduletime` and `completetime`. |
| `cron`, `tz`, `duration_secs` | A recurring window: the 5-field crontab of `[[record.schedule]]` (`min hour dom month dow`), evaluated in `tz` (IANA name, default UTC), each window `duration_secs` long. Parsed and evaluated by the same code as `[[record.schedule]]`. |
| `inhibit` | Stream patterns, as in `streams`. While any live stream matches one, the rule pushes nothing. A pattern that also matches the rule's own streams inhibits the rule while they are live, which is never useful; it is not refused. |

`caudal check` refuses `stop` at or before `start`, an instant that is not
RFC 3339, a bad `cron` or `tz`, `cron` without `duration_secs`, `cron`
together with `start`/`stop`, `stop` together with `duration_secs`,
`duration_secs = 0`, `duration_secs` alone, `tz` without `cron`, and an
`inhibit` pattern that is not a name, `prefix*` or `*`.

How it runs: the same recompute that reacts to publishes and ends also runs
on a timer at the next window boundary of any rule (and at least once a
minute, so a wall-clock jump is noticed within a minute). Opening or closing a window
adds or removes only that rule's targets; the engines diff by target, so
every other push keeps its connection. A push stops at `stop` even if the
stream is still live; a stream that goes live in the middle of an open
window is pushed at once. A recurring window whose `duration_secs` is
longer than the gap between two occurrences behaves as in
`[[record.schedule]]`: the earlier window's end closes the gate.

Chosen conditions, and why only these: `enabled` (with the API toggle)
answers "stop that push now, without editing the file"; `inhibit` covers the
common backup-feed case (push B only while A is down) and needs only the
live stream list the recompute already has. Conditions on track content
("only while it has video") or viewer counts would need the recompute to
run on track changes or poll stats, and MistServer's general
`start_rule`/`end_rule` (`[variable, operator, value]`) needs its custom
variables, which Caudal does not have; none of these is implemented.

`caudal check` (and every reload) refuses a rule with an empty `streams`, a
pattern that is not a name, `prefix*` or `*`, an unknown scheme, a template
that does not expand to a URL with a host (and, for SRT, a port) for a sample
name, `ttl`/`interface`/`pacing` on a non-UDP rule, `token` on a non-WHIP rule, or a UDP destination that
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

- `GET /api/v1/auto-pushes`: each rule with what it runs now, and, when it
  runs nothing, why (`idle`):

  ```json
  [{"index": 0, "streams": ["cam*", "main"], "url": "srt://203.0.113.4:9000?streamid=****",
    "kind": "srt", "enabled": true, "schedule": null, "inhibit": [], "idle": null,
    "active": [{"stream": "cam1", "url": "srt://203.0.113.4:9000?streamid=****"}],
    "skipped": []},
   {"index": 1, "streams": ["show"], "url": "srt://203.0.113.4:9000?streamid=****",
    "kind": "srt", "enabled": true, "inhibit": [],
    "schedule": {"open": false,
                 "window": {"start": "2026-12-21T22:00:00Z", "stop": "2026-12-21T23:00:00Z"}},
    "idle": {"reason": "outside_window",
             "detail": "outside its schedule; the next window opens at 2026-12-21T22:00:00Z"},
    "active": [], "skipped": []}]
  ```

  `idle.reason` is `disabled`, `outside_window`, `inhibited` or `offline`
  (no matching stream is live), the first closed gate in that order.
  `schedule.window` is the window open now, or the next one; its `start` or
  `stop` is `null` when that side is open-ended, and `window` is `null` when
  a one-off window has closed for good. Times are UTC.

- `POST /api/v1/auto-pushes/{index}/enable` and `.../disable`: overrides the
  rule's `enabled` at runtime and returns the rule as above (`404` for no
  such index). The file is not written. The override is kept across
  reloads that leave that rule unchanged, and dropped when the rule is
  edited in the file (even only its `enabled`) or the process restarts.
  `index` is the position in `[[push.auto]]`, so check it in the listing
  after editing the file.

- `GET /api/v1/pushes` keeps its shape (`[{"stream", "url", "section"}]`) and
  adds one row per rule pattern with `"section": "[[push.auto]]"` and the
  template as `url`. Those rows are read-only there: `DELETE
  /api/v1/pushes/{stream}` removes static pushes only; edit the file for a
  rule.
- The running pushes appear where their engine reports: RTMP in
  `GET /api/v1/restreams`, UDP/RTP in `GET /api/v1/multicast` and
  `caudal_multicast_*`.

Secrets are masked in every listing as for static pushes: the last path
segment of an RTMP URL (the stream key), SRT `passphrase` and `streamid`,
a WHIP URL's key-like path segments and query values; `token` is never
listed.

## Not yet

MistServer's general `start_rule`/`end_rule` conditions (`[variable,
operator, value]`) are not implemented, nor conditions on tracks or viewer
counts (see above). `inhibit` here takes stream patterns; whether this
matches MistServer's `inhibit` semantics exactly was not checked against a
running MistServer. Runtime overrides of
`enabled` do not survive a restart. Adding rules from the API is not
implemented either.
