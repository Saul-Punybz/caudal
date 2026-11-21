# `caudal import-mist`: migrate a MistServer config

```sh
caudal import-mist /etc/mistserver.conf -o caudal.toml
```

Reads MistServer's `config.json` (the packages call it `mistserver.conf`;
it is JSON either way) and writes a `caudal.toml`, then prints a report
with every setting in one of three groups:

- **Translated**: same behaviour.
- **Approximated**: imported, but something differs (a URL path, a payload
  format, a global setting instead of a per-stream one). Read these before
  going live.
- **Not imported**: Caudal has no equivalent, or its closest feature means
  something different. These keys are left out of the TOML on purpose, so
  the file never does something MistServer did not.

The generated file is checked with the same rules as `caudal check` before
it is written; if that check fails, nothing is written. Each pull and push
URL is also checked on its own first, so a URL Caudal cannot use becomes a
report line instead of failing the import.

The key names come from MistServer's source, not its documentation (there
is no schema): see `crates/caudal/src/import_mist.rs` and
`docs/research/MISTSERVER-REVERSE.md` §2.2, §3, §4.2, §4.3.

## Connectors (`config.protocols`)

| MistServer connector | Caudal | Notes |
|---|---|---|
| `HTTP` | `[server] http_bind` | on 127.0.0.1 unless MistServer bound loopback: without `[admin]` Caudal refuses a public address. Set up `[admin]`, then widen it |
| `HTTPS` with `cert` + `key` | `[tls] bind`, `cert`, `key` | same loopback rule; without both paths, not imported (set `cert`/`key` or `acme_domains` by hand) |
| `RTMP` | `[rtmp] bind` | publish URL is `rtmp://host/live/<stream>` |
| `TSSRT` | `[srt] bind`, `passphrase` | |
| `RTSP` | `[rtsp] bind` | |
| `WebRTC` | `[webrtc] udp_bind` | WHIP/WHEP only, no WebSocket signalling |
| `HLS` | `[hls] ts` (on) | TS-HLS is `/hls/{name}/ts.m3u8` in Caudal (MistServer: `index.m3u8`). A protocol list **without** `HLS` writes `ts = false` |
| `CMAF` | LL-HLS, always on | `/hls/{name}/master.m3u8`; DASH and Smooth from this connector are not imported |
| `HTTPTS` | always on | `/ts/{name}.ts` (MistServer: `/{name}.ts`) |
| `AAC`, `MP3` | always on | `/audio/{name}.aac`, `/audio/{name}.mp3`, passthrough only |
| `JPG` | always on | `/api/v1/streams/{name}/thumbnail.jpg` |
| anything else (`TS`, `DTSC`, `TSRIST`, `MP4`, `EBML`, `WAV`, `FLAC`, `OGG`, `SubRip`, `HDS`, ...) | not imported | no such output in Caudal yet, or never ported (Flash, HDS, Smooth) |

A connector's `interface` (when it is an IP address) becomes the bind
address; otherwise Caudal binds all interfaces, as MistServer does. A
second connector of the same kind is not imported: Caudal has one bind per
protocol.

## Streams (`streams.<name>`)

| `source` | Caudal |
|---|---|
| empty, `push://` | publish open to anyone (Caudal's default) |
| `push://10.0.0.5` (IP or CIDR) | `[[access.rules]]` `publish_allow` |
| `push://host@password` | the IP part as above; the password is not imported (use `[auth]` tokens or `[auth.hook]`) |
| `srt://host:port?streamid=...` (caller) | `[[srt.pull]]`; options other than `streamid`, `passphrase`, `latency` are reported |
| `srt://:port`, `srt://0.0.0.0:port`, `mode=listener`, `mode=rendezvous` | not imported: Caudal's SRT listener is the shared `[srt] bind` (publish with streamid `publish/<stream>`), and it has no rendezvous |
| `tsudp://group:port`, `tsudp://:port`, `udp://...` | `[[udp.pull]]` as `udp://...`; a hostname instead of an IP, or a second unicast pull on one port, is reported |
| `http-ts://...`, `https-ts://...`, `http(s)://....ts` | `[[http.pull]]` |
| `http(s)://....m3u8`, `http-hls://...` | `[[hls.pull]]` (MPEG-TS segments) |
| `rtmp://`, `rtmps://` | `[[rtmp.pull]]` |
| `rtsp://`, `rtsps://` | `[[rtsp.pull]]` |
| a local file or folder | `[[channel]]` (a looping 24/7 stream, not on-demand VoD) |
| http(s) VoD files (`.mp4`, `.mkv`, ...), `rist://`, `dtsc://`, `*.sdp`, `*-exec:`, `v4l2:`, `balance:`, S3 | not imported |

`DVR` (milliseconds) becomes `[buffer] window_secs`, the largest across all
streams (Caudal's window is global). `always_on` on a pulled stream is
translated: Caudal's pulls always run. Other per-stream keys are reported.
A stream name Caudal does not accept (`A-Z a-z 0-9 _ - . +`, at most 128)
is reported and skipped.

## Pushes (`auto_push`)

| Target | Caudal |
|---|---|
| `rtmp://`, `rtmps://`, `srt://` | `[[push.auto]]` `streams = ["<stream>"]`, same URL (`$stream`, `$basename` work) |
| `tsudp://host:port`, `tsrtp://host:port` | `[[push.auto]]` with `udp://` / `rtp://` (unicast or multicast) |
| a file path | `[record]` `streams += ["<stream>"]` (Caudal picks the file layout: `<dir>/<stream>/<timestamp>/`) |
| `whip://`, `tstcp://`, `rist://`, `cmaf://`, HTTP | not imported |

Also not imported: `#tag` and `name+` stream selectors, and targets that
use MistServer-only variables (`$datetime`, `$year`, `$wildcard`, `$uuid`,
...), since `[[push.auto]]` would send them literally.

Schedules and conditions:

| MistServer | Caudal |
|---|---|
| file target with `scheduletime` + `completetime` | `[[record.schedule]]` `start` (RFC 3339, UTC) + `duration_secs` |
| network target with `scheduletime` / `completetime` | `[[push.auto]]` with `start` / `stop` (UTC): it pushes only inside the window. MistServer's `doAutoPush` compares `scheduletime` the wrong way round; the rule uses the documented meaning |
| `start_rule`, `end_rule`, `inhibit` | not imported (the rule could push when MistServer would not). `inhibit` is left out until its semantics are checked against a real MistServer; `[[push.auto]] inhibit` exists if you want to set it by hand |

MistServer's `doAutoPush` compares `scheduletime` the wrong way round (a
time in the future starts at once, `docs/research/MISTSERVER-REVERSE.md`
§8). The import uses the documented meaning: start at `scheduletime`.

The legacy positional `autopushes` array is not guessed at: start that
MistServer once (it upgrades the array to `auto_push`) and export again.

## Triggers (`config.triggers`)

| Trigger | Caudal |
|---|---|
| `STREAM_PUSH` (blocking) | `[auth.hook]` `publish = true` |
| `USER_NEW`, `CONN_PLAY` (blocking) | `[auth.hook]` `play = true` |
| `PUSH_REWRITE` | `[auth.rewrite]` `publish = true` |
| `PLAY_REWRITE` | `[auth.rewrite]` `play = true` |
| `RTMP_PUSH_REWRITE` | not imported (rewrites the whole URL; port it to `[auth.rewrite]` by hand) |
| `STREAM_SOURCE` | not imported (Caudal starts no input on demand) |
| `STREAM_BUFFER`, `LIVE_BANDWIDTH` | not imported; `[health]` alerts are the closest |
| `STREAM_END`, `USER_END`, `PUSH_END`, `RECORDING_END`, ... | not imported; `[hooks]` notifies on stream.started / stream.ended |
| any other | not imported |

Only blocking (`sync`) HTTP handlers are imported. Executable handlers,
non-blocking ones, a second handler on the same trigger, and a handler on
another URL than the one already imported are reported: each Caudal hook
has one `url` for publish and play. A handler's `streams` filter is
reported (Caudal asks for every stream; answer 2xx for the others), and so
are `params`.

**The handlers need porting.** MistServer POSTs `text/plain` lines
(stream, ip, connector, url, ...) and reads the body. Caudal POSTs signed
JSON (Standard Webhooks; a new `whsec_` secret is generated into the TOML)
and:

- `[auth.hook]` is written with `reply = "mistserver"`: a 2xx allows only
  when the body reads as true, with MistServer's own rule, so a handler
  answering `200` `false` still denies ([AUTH-HOOK.md](AUTH-HOOK.md)). The
  request body still differs (signed JSON, not text lines).
- `[auth.rewrite]` reads a plain-text stream name (empty denies), as
  MistServer does, or JSON `{"stream": ...}`
  ([REWRITE-HOOK.md](REWRITE-HOOK.md)).

`on_error` for `[auth.hook]` follows MistServer: a handler with
`default: "false"` fails closed (`deny`), one without fails open
(`allow`), because MistServer uses `"true"` when a handler does not answer.
`[auth.rewrite]` always gets `on_error = "deny"`. `USER_NEW` fires once per
session and only without a valid token in MistServer; `[auth.hook]` asks
on every play after the token check, cached for `allow_cache_secs`.

## Accounts

`account` users are never migrated: MistServer stores MD5, Caudal uses
argon2id. The TOML ends with a commented `[admin]` block; hash each new
password with `caudal hash-password`.
