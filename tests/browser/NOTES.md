# Browser suite — notes (agent E, batch 2)

## What the suite does
`tests/browser/tests/play.spec.ts` starts the real `caudal` release binary
(`$CAUDAL_BIN`, default `../../target/release/caudal`), starts a real
`ffmpeg` publishing a synthetic 1280x720/30fps + 440Hz test signal over RTMP
to it, then opens `http://127.0.0.1:<http>/play/e2e` in a real (headless)
Chromium and WebKit browser via Playwright and checks that the `<video>`
element actually plays: `readyState >= 3`, `videoWidth == 1280`, and
`currentTime` really advancing (>= 2s over a 3s wall-clock window). It then
measures and asserts live-edge distance and "ingest-to-glass" latency, both
under 3s, and writes the numbers to `results/latest.json`.

Playwright's headless browser *contexts* do not have the "hidden tab, timers
throttled" problem that blocked Chrome-under-automation in batch 1 (see
STATUS.md, "Not built in batch 1" / "PARTIAL" note on piece 6) — a Playwright
page is a real rendered page with real timers running, so hls.js attaches
and native HLS plays normally.

## Exposing hls.js without touching play.html
`crates/caudal-hls/static/play.html` keeps its `hls.js` instance in a local
variable inside an IIFE; nothing is assigned to `window`. Rather than edit
the page (out of scope for this agent — files touched are `tests/browser/**`
and `.github/workflows/browser.yml` only), the test installs a Playwright
`addInitScript` **before** navigation that replaces `window.Hls` with a
`Proxy` whose `construct` trap stashes the real instance on
`window.__hlsInstance` and returns it unchanged. Static access (`Hls.Events`,
`Hls.isSupported()`, `Hls.version`) still forwards to the real class through
the Proxy's default trap, so play.html behaves exactly as it does for a real
visitor; only the test can see the instance afterwards, via
`window.__hlsInstance`. WebKit's native-HLS path never creates an `Hls`
instance (that's expected and handled — see the `video.getStartDate()`
branch in `readMetrics()`), so the exposure only applies to Chromium.

**Suggested (optional) change to `play.html`:** add one line,
`window.hls = hls;` (or a debug-only `window.__caudal = { hls, video };`),
right after `hls = new Hls(...)` in `startHlsJs()`. It would let a person
open devtools and inspect the player directly, and would make this test's
Proxy trick unnecessary. Not required — the trick works today with the page
unmodified — but worth it for someone debugging playback manually.

## Result: Chromium passes; WebKit fails the <3s latency assertion — a real finding, not a test bug

Two runs, both browsers, same numbers within noise:

| | Chromium (hls.js) | WebKit (native HLS) |
|---|---|---|
| readyState | 4 | 4 |
| videoWidth | 1280 | 1280 |
| currentTime delta / 3s | ~3.0s | ~3.0s |
| live-edge distance | ~2.2–2.4s | **-1.4 to -2.1s** (see below) |
| ingest-to-glass | ~2.2–2.4s | **~5.5–6.0s, steady state** |

Playback itself is proven on both engines: the video decodes, renders at
the right resolution, and the clock genuinely advances. The **<3s latency
assertion is not met on WebKit**, and it is not a flaky number — a 40s
manual sampling loop (20 samples, 2s apart) shows WebKit's ingest-to-glass
locked at **5.45s ± 0.001s** for the whole run: it converges once, early,
and never gets closer to live.

**Root cause (confirmed by network trace, not guessed):** WebKit's native
LL-HLS client starts out doing real low-latency, part-based blocking reload
— `GET /hls/e2e/index.m3u8?_HLS_msn=26&_HLS_part=7`, fetching individual
`s26.p0.m4s` … `s26.p10.m4s` parts — but after the first segment or two it
**stops requesting parts and falls back to whole-segment blocking reload**
(`?_HLS_msn=27`, `?_HLS_msn=28`, …, fetching `s27.m4s` etc. as whole 2s
segments). Once in that mode it never asks for a part again. That fallback
converges to the HLS spec's *default* live-edge distance for a playlist
without an `EXT-X-HOLD-BACK` (only `PART-HOLD-BACK`), which is
`3 × EXT-X-TARGETDURATION` = `3 × 2s` = `6s` — consistent with the observed
~5.45–5.96s.

This lines up exactly with a limitation STATUS.md already names: the
LL-HLS output has a known MUST-level mediastreamvalidator finding,
`-50120 Content not delivered via HTTP/2`, explicitly scheduled for removal
by **M7 (TLS/ACME)**, not batch 2. Caudal currently serves plain HTTP/1.1.
Apple's own LL-HLS conformance requirements make HTTP/2 effectively load-
bearing for sustained part-based delivery; without it, WebKit's client
reverts to conventional segment-level HLS after the initial parts, and the
3-segment default hold-back applies. This is a server-transport issue, not
a `play.html` bug and not something fixable inside `tests/browser/**`.

The negative live-edge distance on WebKit (`seekable.end() - currentTime` a
small negative number, e.g. -2.1s) is a side effect of the same fallback:
WebKit only extends `video.seekable`'s end at whole-segment boundaries, so
`currentTime` — which advances continuously within the segment currently
playing — can read slightly past the last *reported* seekable point. It
still satisfies the "< 3" assertion since it's a small number, but it is
not really "distance to live edge" once WebKit is off low-latency delivery;
it is closer to zero than 6s only by nature of the metric, not because
WebKit is actually near the live edge (ingest-to-glass, computed from
`EXT-X-PROGRAM-DATE-TIME`, is the trustworthy number here).

**What this means for `npm test`:** the Chromium project passes cleanly on
every run. The WebKit project's single test fails on the `ingestToGlassSec
< 3` assertion (and would on `liveEdgeDistanceSec` too, if WebKit's number
weren't coincidentally negative). This is left failing on purpose — the
brief asks the suite to assert `< 3s` for both, and papering over a real,
reproduced, root-caused ~5.5s WebKit number would hide the actual state of
LL-HLS delivery from the orchestrator. `.github/workflows/browser.yml` will
show this job red on `main` until M7 (HTTP/2) lands; that is accurate, not
a CI bug.

## Other things worth knowing
- The suite skips entirely (`test.skip`) with a visible reason if `ffmpeg`
  is not on `PATH`.
- It fails fast with an actionable message if `$CAUDAL_BIN` (or the default
  `../../target/release/caudal`) doesn't exist, telling the user to run
  `cargo build --release -p caudal`.
- Each Playwright "project" (chromium, webkit) gets its own `caudal`
  process, its own free TCP ports (`server`/`rtmp`) and its own free UDP
  port (`srt`, bound the same way the config expects — unused by this
  suite, just present so the config matches the fixed key set in
  STATUS.md), and its own `ffmpeg` publish to stream name `e2e`. No shared
  state between projects.
- `results/latest.json` is written per-browser and merged (both browsers'
  numbers land in one file after a full `npm test` run) — it is gitignored,
  written fresh each run.
- The Chromium project passes `--autoplay-policy=no-user-gesture-required`;
  play.html's `<video>` is already `muted autoplay`, so in practice neither
  browser needed the Playwright fallback click — the test still checks for
  it and would click play + log a line if a future change to play.html
  ever required a gesture.

## Firefox joined late and never caught up (21 Sep 2026)

`play.spec.ts` failed on the **firefox** project only, repeatedly, two ways:
"playback stalled after warm-up" (`currentTime` moved ~0.25 s in 3 s) and
"ingest-to-glass" 3.6–4.7 s against a 3 s bound. Chromium and WebKit passed
in the same runs. It was put down to runner load and given one CI retry.

It was not the runner. A firefox-only debug run (`--repeat-each=5`, hls.js
`debug` on, a 500 ms sampler) showed the same shape all five times:

```
0.8 ct=1.770 rs=3 paused=true  buf=[0.00-1.17] lat=1.00 tgt=0.60
...                                   (nothing moves for 3-6 s)
5.8 ct=1.770 rs=3 paused=true  buf=[0.00-3.99] lat=6.00 tgt=0.60
6.3 ct=2.135 rs=4 paused=false buf=[0.00-7.98] lat=6.27 tgt=0.60
```

Three facts, in order:

1. **Firefox honours the `autoplay` attribute only at `HAVE_ENOUGH_DATA`
   (readyState 4)**, which took 2.8–6.4 s here; Chromium and WebKit start at
   `HAVE_FUTURE_DATA` (3). Until then the `<video>` sits paused.
2. **`currentTime` is already past 1 s while it is paused**, because hls.js
   seeks the element to the live-sync position as soon as it attaches. So the
   old warm-up gate (`currentTime > 1`) passed on a video that had never
   played, and the 3 s window that followed measured a paused element — that
   is the "stall". Nothing was stalling; nothing had started.
3. **hls.js cannot recover from a late join.** Its catch-up
   (`maxLiveSyncPlaybackRate`) only engages while
   `latency - targetLatency < targetLatency + targetduration` — here
   0.601 + 2 = 2.6 s — and past that it treats the playhead as DVR playback
   and leaves it alone (`latency-controller.ts`, `inLiveRange`). Its
   seek-back to the live edge is off by default
   (`liveMaxLatencyDurationCount: Infinity`, so `synchronizeToLiveEdge`'s
   `currentTime < end - maxLatency` is never true). A player that joins more
   than ~2.6 s late stays exactly that far behind for the whole session —
   the debug run sat at 6.3 s of latency for 19 s straight, and the CI
   failures sat at 3.70 s.

The playlist was not at fault: the dumps at the stall are ordinary LL-HLS
(one `EXT-X-MAP`, no discontinuity, parts for the last three segments, the
preload hint present), and Caudal's own numbers are unchanged. The server
side of PR #6 is exonerated; the only reason the failures started around it
is that the Firefox project and the extra specs before it moved the timing.

**Fixed in `crates/caudal-hls/static/play.html`**, because both halves are
real for a viewer, not just for CI:

- the page calls `video.play()` itself on `loadeddata`/`canplay` instead of
  waiting for Firefox's `HAVE_ENOUGH_DATA`;
- a 250 ms guard seeks to `hls.liveSyncPosition` when the playhead has been
  stranded outside hls.js' catch-up range for 2 s.

**And in the spec**, which was measuring the wrong thing: warm-up now waits
for `currentTime` to actually advance while the video is unpaused, and the
stall message carries the measurement. The firefox CI retry is gone.
