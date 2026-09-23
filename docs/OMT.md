# Open Media Transport (OMT)

OMT is an open, low-latency video protocol for local networks (the one vMix
and the OBS OMT plugin speak): VMX-compressed video, float audio and XML
metadata over TCP, sources found by mDNS (`_omt._tcp`) or a discovery server.
Milestone 12 adds it to Caudal in both directions:

- **`[[omt.pull]]`**: receive an OMT source and publish it as a normal Caudal
  stream. VMX is decoded in Rust (`vmx-codec`); ffmpeg, run as a separate
  process, encodes H.264 + AAC. From there the stream goes wherever any
  stream goes: LL-HLS, WebRTC, SRT, RTMP restream, recording, transcode.
- **`[[omt.output]]`**: send any H.264 stream (an ingest or a transcode
  rendition, `name+label`) out as an OMT source that vMix/OBS can pick.
- A source list (`GET /api/v1/omt/sources`), per-stream OMT fields in the
  stream API, `caudal_omt_*` metrics, tally both ways, and live reload.

The protocol crates (`open-media-transport`, `vmx-codec`) are our own, MIT OR
Apache-2.0, pure Rust, pinned to a commit of
[Saul-Punybz/open-media-transport](https://github.com/Saul-Punybz/open-media-transport)
while M12 is developed (crates.io before a release).

## Config

```toml
[omt]
# ffmpeg = "ffmpeg"                   # default: [transcode] ffmpeg
# discovery_server = "omt://10.0.0.2" # omt://host[:port] (6399); absent = mDNS only
# interfaces = ["en0"]                # mDNS only on these (name or address); empty = all
# exclude_interfaces = ["bridge100"]

[[omt.pull]]
stream = "cam1"
source = "STUDIO-PC (Camera 1)"      # full name, as /api/v1/omt/sources lists it
quality = "high"                     # default | low | medium | high (a suggestion to the sender)
video_kbps = 6000                    # H.264 bitrate published (default 6000)
audio_kbps = 128                     # AAC bitrate (default 128)

[[omt.pull]]
stream = "cam2"
url = "omt://10.0.0.7:6400"          # an address instead of a name
allowed_sources = ["10.0.0.0/24"]

[[omt.output]]
stream = "program"                   # any H.264 stream, or a rendition "program+720p"
name = "Caudal Program"              # announced as "MACHINE (Caudal Program)"
quality = "default"                  # default follows what receivers ask for
encoder_threads = 0                  # VMX encoder threads; 0 = from the frame size
```

### `[omt]`

| key | default | |
|---|---|---|
| `ffmpeg` | `[transcode] ffmpeg` | The ffmpeg every pull runs. `caudal doctor` checks it when a pull exists. |
| `discovery_server` | none | `omt://host[:port]` (port 6399 when absent). Sources are found through it and outputs register with it instead of announcing over mDNS; mDNS browsing stays on. For networks without multicast. |
| `interfaces` | all | mDNS only on these interfaces: names (`en0`, `eth1`) or an address on the interface. Loopback is never used. |
| `exclude_interfaces` | none | Never use these for mDNS. |

All pulls and outputs share **one** discovery (one mDNS responder, one
directory of sources). It starts when something needs it: a pull by name or
any output (at startup or on a reload), or the first
`GET /api/v1/omt/sources`. A server with no OMT config never opens the mDNS
socket. If it cannot start (no multicast-capable interface, a bad server
host), the error is logged, pulls by `url` still work, and the next use
tries again.

### `[[omt.pull]]`

| key | default | |
|---|---|---|
| `stream` | required | Stream name to publish. Unique among pulls, and not also a `[[channel]]`, `[[failover]]` or `[[rtsp.pull]]` stream. |
| `source` / `url` | one required | `source`: the full name `MACHINE (Name)`. `url`: `omt://host:port`. Exactly one. |
| `quality` | `default` | Suggested to the sender with the video subscription. |
| `video_kbps` | 6000 | 100..=200000. |
| `audio_kbps` | 128 | 16..=1024. |
| `follow_redirects` | `false` | Follow a redirect the source sends (OMT §9). See Security: **not enforced yet**. |
| `allowed_sources` | any | CIDRs (a bare address is one host) the pull may connect to. A `url` with a literal IP outside them is a config error; for names, see Security: **not enforced yet**. |

What a pull does, briefly (details in `crates/caudal-omt/src/ingest.rs`):
one thread per pull; the newest picture is decoded and older queued ones
dropped when behind (VMX is intra-only, any picture can go); a full ffmpeg
queue drops rather than blocks. A short source outage (under 5 s) keeps the
stream published and re-anchors the clock; longer ends it, and the next
frame publishes it again. Tally goes to the source: program while the
stream has viewers, preview while it is published.

### `[[omt.output]]`

| key | default | |
|---|---|---|
| `stream` | required | An H.264 stream (codec handling is documented when the output lands). The same stream may go out under several names. |
| `name` | required | 1-128 characters, no parentheses or control characters, unique among outputs. |
| `quality` | `default` | VMX encoder quality; `default` follows receivers' suggestions. |
| `encoder_threads` | 0 | 0..=64; 0 picks from the frame size, as libomtnet does. |
| `bind` | all interfaces | Listen on this address only. **Not enforced yet.** |
| `allow` | any | CIDRs receivers may connect from. **Not enforced yet.** |
| `max_connections` | no cap | 1..=1000 receiver connections at once. **Not enforced yet.** |

### Reload

`POST /api/v1/config/reload` (or SIGHUP) applies `[[omt.pull]]`,
`[[omt.output]]` and the pulls' ffmpeg live: an unchanged pull or output
keeps running untouched, removed ones stop (a pull's stream ends, an
output's source is withdrawn), changed or new ones start. The reload report
lists them as `omt.pull` / `omt.output`. The discovery keys
(`discovery_server`, `interfaces`, `exclude_interfaces`) apply live only
while discovery has not started; after that they are reported under
`requires_restart` as `omt.discovery`.

## API

`GET /api/v1/omt/sources` (behind `[admin]` login like the rest of
`/api/v1`): the sources discovered now, sorted by name. The first call that
starts discovery waits 1.5 s for answers.

```json
[{"name": "STUDIO-PC (Camera 1)", "host": "studio-pc.local.", "port": 6400,
  "addresses": ["192.168.1.20"], "url": "omt://192.168.1.20:6400"}]
```

`name` is what `source` takes, `url` what `url` takes. 503 with
`{"error": ...}` when discovery cannot start.

`GET /api/v1/streams/{name}` (and each entry of `/api/v1/streams`) gains an
`omt` object on streams a pull publishes or an output sends; other streams
are unchanged:

```json
"omt": {
  "pull": {"source": "STUDIO-PC (Camera 1)", "connected": true,
           "video_in": 3600, "audio_in": 3600, "bytes_in": 214000000,
           "reconnects": 0, "publishes": 1,
           "dropped": {"behind": 0, "busy": 0, "decode": 0, "invalid": 0, "queue_full": 0, "timing": 0},
           "tally": {"preview": true, "program": true}},
  "outputs": [{"name": "Caudal Program", "full_name": "CAUDAL-HOST (Caudal Program)",
               "url": "omt://CAUDAL-HOST:6401", "receivers": 1, "frames_sent": 3600,
               "dropped": {...}, "tally": {"preview": false, "program": true}}]
}
```

## Metrics

Pulls are labelled `stream`; outputs `stream` and `output` (the source name).

| metric | type | |
|---|---|---|
| `caudal_omt_frames_in_total{stream,kind}` | counter | Frames received, `kind` = `video` / `audio`. |
| `caudal_omt_bytes_in_total{stream}` | counter | Bytes received. |
| `caudal_omt_reconnects_total{stream}` | counter | Reconnections after the first connect. |
| `caudal_omt_publishes_total{stream}` | counter | Times the pull (re)published its stream. |
| `caudal_omt_connected{stream}` | gauge | 1 while connected. |
| `caudal_omt_frames_out_total{stream,output}` | counter | Video frames sent. |
| `caudal_omt_receivers{stream,output}` | gauge | Video receivers now. |
| `caudal_omt_tally{direction,stream,[output,]state}` | gauge | 1 while on `preview`/`program`: for a pull what Caudal tells the source, for an output what receivers tell Caudal. |
| `caudal_omt_frames_dropped_total{direction,stream,[output,]reason}` | counter | Pull reasons: `behind`, `queue_full`, `decode`, `invalid`, `timing`, `busy`. |

## Security

**OMT has no authentication and no encryption.** Anyone who can reach a
sender's port can watch it; anyone who can reach a receiver's source can
feed it; mDNS answers anyone on the segment. It is a trusted-LAN protocol,
like NDI.

- Keep OMT on a production LAN or VLAN. Firewall it off from everything
  else. **Never expose TCP 6400-6600 (senders) or 6399 (discovery server) to
  the internet.**
- Across a WAN, don't tunnel OMT: pull it into Caudal on the LAN and carry
  it with Caudal's own protocols, which do authenticate: SRT with a
  passphrase, WebRTC (WHIP/WHEP) or LL-HLS with `[auth]` tokens, and
  `[[access.rules]]` for IP/country limits.
- Treat a pulled source as untrusted input. The VMX decoder runs on
  whatever the network sends; it is fuzzed (`fuzz/`, `omt_vmx_decode`, in
  CI on every push) and its errors drop the frame, never the server.

**What is not enforced yet.** The config accepts, validates and documents
these, but the pinned OMT library (rev `8c275ff`) cannot apply them; the
server logs a warning for each at startup and on reload:

| setting | what happens today | needs |
|---|---|---|
| pull `follow_redirects = false` (the default) | Redirects **are** followed: a source (or anyone who can talk to its port) can send Caudal to another source, which then plays on Caudal's public outputs. | A redirect policy in the library, then `caudal_omt::PullConfig` passing it. |
| pull `allowed_sources` with a `source` name | A name resolves to whatever mDNS or the discovery server says. (A `url` with an IP outside the list is refused at config check.) | A resolution hook / address filter in the library. |
| output `bind` | Listens on every interface. | Sender bind address in the library. |
| output `allow` | Any receiver may connect. | Sender allow-list in the library. |
| output `max_connections` | No cap. | Sender connection cap in the library. |

Until then, the firewall/VLAN is the control. The code is marked
`TODO(omt-security)` (`crates/caudal/src/config.rs`, `omt_unenforced`).

## Formats and limits

- Pulls decode VMX to 8-bit UYVY; ffmpeg converts to 4:2:0 for H.264. A
  10-bit source arrives as 8-bit (VMX's 8-bit decode path, as libomtnet
  does for that preference); alpha is dropped. **10-bit and alpha are not
  carried yet.** Audio: up to 8 channels.
- Outputs are designed for H.264 streams only for now (no HEVC/AV1); the
  output is still being built, so its exact audio handling is documented
  when it lands.
- No VMX passthrough (pull → output without decoding), no relay, no VMX
  recording, no discovery-server mode inside Caudal, no UI picker: later.

## What is verified, and what is not

Verified (and how):

- Config parsing and every validation error, the reload diff, the API
  shapes and the metrics text: unit tests in `crates/caudal` (the output
  side against a placeholder until `output.rs` lands).
- The pull, on one Mac (Apple M4): loopback tests against our own sender
  (codecs, timestamps, A/V offset 12 ms, reconnect keeping the stream,
  reload, tally) and one external check: a **libomtnet** sender (the .NET
  reference implementation, through the OMT repo's harness) into the pull,
  640x360 30 fps; the published H.264/AAC decoded cleanly with ffmpeg, and
  our tally reached libomtnet. See `crates/caudal-omt/NOTES.md`.
- The VMX decoder under fuzzing: Caudal's `omt_vmx_decode` target, 30 s
  locally (650 k runs, no failure) plus the OMT repo's own target.

Not verified:

- **vMix, OBS (OMT plugin), a Raspberry Pi or any other real OMT product.**
  Only libomtnet, on one Mac.
- 1080p60 end to end (the `bench.yml` OMT scenarios below measure it on
  GitHub's Linux runners; no result yet), and Linux in general beyond CI.
- Pull by name through mDNS inside Caudal (the protocol crate's own
  addressing was verified in the OMT repo, not through Caudal).
- The output end to end (being built on its own branch; this document is
  updated when it lands).
- 10-bit and alpha (not supported), and everything in the Security table
  above (not enforced).

## Benchmarks and soak

- **CPU at 1080p60**: `bench.yml` with input `omt = ingest,output` runs
  `bench/omt.py` on a GitHub Linux runner (never on a laptop). *ingest*:
  `omt send` 1920x1080 at 60 fps → `[[omt.pull]]` by URL → ffmpeg; reports
  the cores used by Caudal plus its ffmpeg, the sender's cores, frames per
  second actually received, and drops by reason. *output*: ffmpeg `testsrc2`
  1080p60 H.264 + AAC over RTMP → `[[omt.output]]` → `omt recv`; reports
  Caudal's cores, frames sent per second and what the receiver saw.
- **Soak (planned, not built)**: `bench/soak.py` is built around RTMP
  publishers and HLS/RTSP/WHEP viewers; OMT churn does not fit it without
  restructuring, so it stays a plan. An `--omt` mode would add one `omt send`
  source pulled by URL and one output with an `omt recv` receiver, restart
  the sender every churn period (the pull must reconnect and keep the stream
  within the 5 s grace, then end and republish when the outage is longer),
  reconnect the receiver, and add `omt.pull`/`omt.output` changes to the
  one reload. Pass rules on top of the existing ones: RSS slope under
  1 MB/h after warm-up (the open v0.1 soak finding is per churn event, so
  OMT churn is exactly what to watch), `caudal_omt_reconnects_total` equal to
  the number of sender restarts, no `decode`/`invalid` drops, thread count
  flat (each pull is a thread; each receiver and sender start their own).
