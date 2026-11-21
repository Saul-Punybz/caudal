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

A pull can be `rtsp://`/`rtsps://` (`[[rtsp.pull]]`), `srt://` (`[[srt.pull]]`, Caudal calls the listener) `udp://`/`rtp://` (`[[udp.pull]]`, MPEG-TS over UDP) or `http://`/`https://` (`[[http.pull]]`, MPEG-TS served over HTTP). A stream can have one pull, across all of them. See `docs/PULL-INPUTS.md`.

## Pushes

| | |
|---|---|
| `GET /api/v1/pushes` | `[{"stream", "url", "section"}]` |
| `POST /api/v1/pushes` `{"stream": "show", "url": "rtmps://live.example/app/KEY"}` | `rtmp://`/`rtmps://` → `[[restream]]`, `srt://` → `[[srt.push]]`, `udp://`/`rtp://` → `[[udp.push]]` (an IP address) |
| `DELETE /api/v1/pushes/{stream}[?url=...]` | removes the stream's pushes (only that url when given) |

A push starts when its stream is live and keeps retrying while it is.

Automatic pushes (`[[push.auto]]`, one rule for every stream matching a
pattern) are listed too, one row per pattern with `"section": "[[push.auto]]"`
and the URL template, but are not added or removed here. `GET
/api/v1/auto-pushes` shows each rule with the pushes it runs now. See
`docs/AUTO-PUSH.md`.

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

## Thumbnails

`GET /api/v1/streams/{name}/thumbnail.jpg[?width=N]` returns a JPEG of the
stream's newest video keyframe, for dashboards and operators. Unlike the
routes above it changes nothing; it is under `/api/` and so needs the admin
login like the rest of the API.

- `width`: 16 to 1280 pixels, default 320. The height keeps the aspect
  ratio (rounded to an even number). A picture is never scaled up past the
  source's own size.
- Video codecs: H.264, H.265 and MPEG-2 video. The keyframe is muxed to
  MPEG-TS with its parameter sets and decoded by ffmpeg: the one from
  `[transcode] ffmpeg` (default `ffmpeg` on `PATH`; read at startup, a
  reload does not change it). No new config keys.
- Caching: one picture per stream and width, reused for 3 s, and for as
  long as the newest keyframe is still the one it shows. Requests that
  arrive while a picture is being made wait for it, so many dashboards cost
  one ffmpeg; at most 2 ffmpeg run at once for all streams, each with an
  8 s limit. Answers carry `Cache-Control: private, max-age=2`.

| Answer | When |
|---|---|
| `200 image/jpeg` | the picture |
| `400` | `width` out of range |
| `404` | no such stream |
| `409` | no video track, or a video codec other than the three above |
| `503` + `Retry-After: 1` | video, but no keyframe yet |
| `503 {"error": ...}` | ffmpeg missing, failed or timed out, or too busy |
