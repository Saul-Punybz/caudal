# Pulls and pushes from the API

Add and remove where streams come from (pulls) and where they go (pushes)
without editing the file by hand. All routes are under `/api/`, so they need
the admin login when `[admin]` is configured.

**The config file stays the source of truth.** Every change is made to the
file the server was started with (`--config`), then applied live exactly as
`POST /api/v1/config/reload` would. A server started without a config file
answers `409`.

How a change is written:

1. The file is read and the change is applied to its text with `toml_edit`:
   your comments, order and formatting stay.
2. The result is parsed and validated as a whole config. If it is invalid
   the file is not touched and the answer is `400` with the reason.
3. It is written to a temporary file next to the original, flushed to disk,
   and renamed over it (the original's permissions are kept). A crash or a
   power cut leaves the old file or the new one, never half of either.
4. The running server reloads from it.

One edit runs at a time.

## Pulls

| | |
|---|---|
| `GET /api/v1/pulls` | `[{"stream", "url", "section"}]` |
| `POST /api/v1/pulls` `{"stream": "cam1", "url": "rtsp://10.0.0.5/live"}` | adds a `[[rtsp.pull]]` |
| `DELETE /api/v1/pulls/{stream}[?url=...]` | removes the stream's pulls (only that url when given) |

A pull can be `rtsp://`/`rtsps://` (`[[rtsp.pull]]`), `srt://` (`[[srt.pull]]`, Caudal calls the listener), `udp://`/`rtp://` (`[[udp.pull]]`, MPEG-TS over UDP), `http://`/`https://` with a path ending in `.m3u8` (`[[hls.pull]]`, an HLS or LL-HLS playlist) or any other `http://`/`https://` (`[[http.pull]]`, MPEG-TS served over HTTP). A stream can have one pull, across all of them. See `docs/PULL-INPUTS.md`.

## Pushes

| | |
|---|---|
| `GET /api/v1/pushes` | `[{"stream", "url", "section"}]` |
| `POST /api/v1/pushes` `{"stream": "show", "url": "rtmps://live.example/app/KEY"}` | `rtmp://`/`rtmps://` → `[[restream]]`, `srt://` → `[[srt.push]]`, `udp://`/`rtp://` → `[[udp.push]]` (an IP address) |
| `DELETE /api/v1/pushes/{stream}[?url=...]` | removes the stream's pushes (only that url when given) |

A push starts when its stream is live and keeps retrying while it is.

## Answers

- `200 {"applied": [...], "restarted": [...], "requires_restart": [...]}`:
  written and applied, the same report as a reload.
- `400` invalid change (bad stream name, wrong scheme, a second pull for the
  same stream...), file untouched.
- `404` nothing to remove. `409` the identical entry already exists, or no
  config file.

Listings mask secrets: an RTMP stream key shows as `****`, so do SRT
`passphrase` / `streamid` values and an RTSP camera's password. The file
itself keeps them; protect it like a password (the server keeps its mode).
