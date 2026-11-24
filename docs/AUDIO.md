# Audio-only output (internet radio)

Every live stream's audio is also served alone, as one endless HTTP
response, the way an internet radio station serves it:

| URL | Format | Content-Type | Source audio it serves |
|---|---|---|---|
| `/audio/{name}.aac` | ADTS AAC | `audio/aac` | AAC (what RTMP and most SRT/RTSP encoders send) |
| `/audio/{name}.mp3` | MPEG audio | `audio/mpeg` | MP3, and MP2 from a broadcast feed |

Any player opens it: VLC (`vlc http://host:8080/audio/oceano.mp3`), a
browser (`<audio src="http://host:8080/audio/oceano.aac" controls>`), a
smart speaker, or an internet radio app / directory listing. No config is
needed; the route is on the main HTTP (and HTTPS) port for every stream.

## Behaviour

- **Passthrough, no decoding.** AAC access units get a 7-byte ADTS header
  each; MPEG audio frames already carry their own header and go out as
  they are. MP2 (Layer II, what ATSC/DVB feeds carry) is valid inside an
  `audio/mpeg` stream, and MP3 decoders read it (verified with ffmpeg;
  other players not yet checked).
- **No transcoding.** A request whose format does not match the stream's
  audio codec gets `415` with the URL that does work (an AAC stream asked
  for as `.mp3` answers "use /audio/{name}.aac"). Opus (WHIP, some SRT
  encoders), AC-3 and other codecs get `415`. To serve those, set the
  encoder to AAC or MP3, or serve a `[[transcode.ladder]]` rendition instead
  (renditions are encoded to AAC, so `/audio/{rendition}.aac` works).
  Transcoding here was left out on purpose: it would cost an ffmpeg per
  listened stream for what is a one-time encoder setting.
- **Live edge.** A listener starts at the newest join point (the newest
  video keyframe on a stream with video, the newest frame on an
  audio-only one) and stays live: one that falls more than 5 seconds
  behind (slow network, paused player) is moved forward to the live edge,
  dropping what it missed. Memory per listener does not grow; frames come
  from the stream's shared buffer.
- **End.** The response ends (chunked body closes) when the publisher
  leaves, or when the stream's audio changes to a codec this URL cannot
  carry. Players reconnect on their own.
- **Several audio tracks:** the first one is served.

## Auth and sessions

Same rules as HLS and the other playback routes: a play token as
`?token=...` or `Authorization: Bearer ...` when `[auth]` or `[auth.hook]`
require one (`401` without, `403` refused), `[[access.rules]]` by the
client address (`[server] trusted_proxies` for `X-Forwarded-For`). A name
nobody publishes is `404`. On a cluster edge the first authorized request
pulls the stream from an origin. The route is public under `[admin]` (it
checks its own tokens).

Each open response is a viewer session with protocol `audio` in
`GET /api/v1/sessions` and the stream's viewer count; kicking it ends the
response (and, with a block, refuses reconnects for that long).

## ICY metadata

Players that send `Icy-MetaData: 1` (VLC, Winamp-style apps, most radio
apps) get SHOUTcast metadata: the response has `icy-metaint: 16000` and
`icy-name: {name}`, and after every 16000 audio bytes a metadata block;
the first carries `StreamTitle='{name}';`, later ones are empty (no
change). Players that do not ask get plain audio.

## Not done

WAV, OGG and FLAC (MistServer serves those) are not offered; PCM or FLAC
sources answer `415`. There is no per-song title: the title is the stream
name.
