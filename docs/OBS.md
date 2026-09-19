# OBS → Caudal → browser

Publish from OBS, watch it in a browser, and measure glass-to-glass
latency (the time from what the camera sees to what the viewer sees).
Assumes a running Caudal server — see [QUICKSTART.md](QUICKSTART.md) first.

## 1. OBS settings

**Settings → Stream:**

- Service: `Custom...`
- Server: `rtmp://<host>:1935/live` — the port and app name come from
  `[rtmp] bind` and `[rtmp] app` in `caudal.toml` (`1935` / `"live"` by
  default; see `caudal.example.toml`).
- Stream Key: the stream name, e.g. `main`. This is what appears in the
  URLs below and in `GET /api/v1/streams`.

**Settings → Output (Advanced mode recommended):**

- Encoder: x264, with **Tune: `zerolatency`** — or a hardware encoder
  (NVENC, QuickSync, Apple VT) if you have one; hardware encoders don't
  expose a `zerolatency` tune but generally add less latency than
  software x264 without it.
- Rate Control: **CBR**
- Keyframe Interval: **2 s** (`Keyframe Interval (frames)` on some OBS
  versions — set it to your fps × 2, e.g. `60` at 30 fps). This must
  match `[hls] segment_ms` in `caudal.toml` (default `2000`, i.e. 2 s):
  Caudal cuts a new HLS segment on each keyframe, so a mismatch makes
  segments the wrong length or breaks LL-HLS's part/segment math.
- **B-Frames: 0** if you'll play the stream back over WHEP (WebRTC). Set
  it under x264's own options (`bf=0`) or, on the Output panel, uncheck
  "Use B-Frames" if your encoder shows it. LL-HLS tolerates B-frames;
  WebRTC's H.264 payloading in Caudal does not reorder frames, so leaving
  B-frames on will visibly corrupt or stutter WHEP playback.

**Settings → Audio:**

- Sample Rate: **48 kHz**
- Format: AAC (OBS's default), any reasonable bitrate (128 kbps is fine)

Click **Start Streaming**. Caudal logs a new publisher and
`GET /api/v1/streams/<name>` (or the UI, below) shows it live within a
couple of seconds.

## 2. Watch it

### Quick check: the built-in player

```
http://<host>:8080/play/<name>
```

A minimal page (hls.js on Chromium/Firefox, native HLS on Safari) that
plays the LL-HLS output and shows **measured latency** (`hls.latency`,
color-coded) and buffer length live in its footer — no login needed
unless `[auth] play = true` is set.

### The admin UI

```
http://<host>:8080/streams/<name>
```

(behind the `[admin]` login if configured — see QUICKSTART.md step 2).
The stream detail page has a three-way toggle: **LL-HLS**, **WebRTC**
(WHEP), and **MoQ** (WebTransport, if `[moq]` is enabled). Each mode shows
its own latency readout: seconds behind live for LL-HLS, an estimated
buffer size in ms for WebRTC and MoQ (not a true glass-to-glass
measurement — see below for that).

## 3. Measure glass-to-glass latency

"Glass-to-glass" is the time from a photon hitting the camera to a pixel
lighting up on the viewer's screen — the number that matters to a person,
not the edge or buffer latency a player reports. Caudal doesn't
instrument this end-to-end yet (see `docs/research/BENCH-MEDIAMTX.md`,
"NOT MEASURED"), so measure it directly:

1. Add a millisecond clock to the OBS scene: a **Browser Source** pointed
   at a page that shows `Date.now()` (or any stopwatch/clock site with
   millisecond digits) works well, since it updates every frame OBS
   renders. A phone stopwatch app held up to the camera works too if you
   don't want to add a source.
2. Open the clock/stopwatch full-screen in the browser tab where you'll
   also open the Caudal player (`/play/<name>` or the UI's WebRTC tab),
   side by side or in two windows on the same screen.
3. Take a photo (phone camera, or a screenshot timed by eye) showing
   **both** the OBS source and the browser playback in the same frame.
4. Read off both timestamps in the photo and subtract:
   `latency = (time shown in browser) − (time shown in OBS source)`.
5. Repeat 5 times and take the **median** (a couple of outliers from
   network jitter are normal; the median is the number to report).

**Pass:** under 3 seconds on LL-HLS.

**Expected** (not yet independently measured end-to-end by the team — this
procedure is how to get that number): roughly 1–1.5 s on LL-HLS in
Chromium/Firefox, sub-second on WebRTC (WHEP). LL-HLS's own edge latency
(server to segment availability, no player buffering) measured at 212 ms
median in `docs/research/BENCH-MEDIAMTX.md`; the player adds its own
hold-back and buffering on top of that, which is most of the 1–1.5 s.

**Safari** only reaches low latency over HTTPS + HTTP/2 (its native HLS
player won't do blocking playlist reloads over plain HTTP/1.1). Configure
`[tls]` in `caudal.toml` — see the commented `[tls]` block in
`caudal.example.toml` (PEM files or automatic ACME) — and use
`https://<host>:8443/play/<name>` from Safari.

## Troubleshooting

- **Run `caudal doctor --config caudal.toml`** first for anything not
  working — it checks the config, ports, WebRTC NAT reachability, TLS
  certs, and more, with a fix for each failure. Add `--url
  http://<host>:8080` once the stream is live to also flag codecs a
  browser can't play.
- **OBS won't connect / "Failed to connect to server":** confirm port
  `1935` (RTMP) is reachable and not already in use —
  `caudal doctor` flags a bind conflict.
- **Player shows nothing / stays at "Loading":** confirm port `8080`
  (HTTP) is reachable, and that the stream name in the URL matches the
  OBS stream key exactly.
- **WHEP (WebRTC) connects but no video, or ICE never completes:**
  WebRTC's media travels over UDP `8189` (`[webrtc] udp_bind`, default
  `0.0.0.0:8189`) — check it isn't blocked by a firewall, and that
  `[webrtc] public_ips` is set if the server is behind 1:1 NAT.
  `caudal doctor` includes a NAT reachability check for this.
- **Safari won't play at low latency:** see the HTTPS/HTTP/2 note above.
