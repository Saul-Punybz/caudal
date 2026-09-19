# Origin-edge clustering (M10, MistServer #190)

Goal: several Caudal servers act as one. A publisher pushes to any origin,
viewers connect to any edge, and an edge pulls a stream from an origin when
its first viewer asks for it. Audience capacity grows by adding edges.

## Design as built

- **Not `moq-relay`** (PLAN.md said clustering would come from it). A relay
  forwards MoQ to MoQ viewers only; an edge here must serve LL-HLS, WHEP,
  RTSP and SRT too, which needs the stream in the edge's own registry. So
  the edge is a small MoQ subscriber (moq-native client, the pinned
  `moq-net`/`hang` already used by `caudal-moq`) that republishes into the
  registry; moq types stay inside `caudal-cluster`, behind the core's
  `Demand` trait.

```toml
[cluster]
role = "edge"                 # "origin" | "edge"
node_id = "edge-1"            # optional; shows up in logs and tokens
secret = "…"                  # shared by every node, >= 16 bytes
origins = ["http://origin-a:8080", "http://origin-b:8080"]   # edge only
idle_timeout_secs = 30        # edge: stop pulling this long after the last viewer
```

- **Origins are HTTP base URLs, not MoQ URLs** (a change from the brief's
  `https://origin-a:4443`). Reason: the edge needs two things only HTTP has,
  the locate answer and the MoQ endpoint's certificate fingerprint
  (`/moq/fingerprint`, which also gives the MoQ URL). One URL per origin is
  enough to reach both.
- **Inter-node auth**: HS256 JWT signed with `secret`, `aud = "caudal-cluster"`,
  `iss = node_id`, five-minute expiry, minted fresh per request. Origins
  check it on `GET /api/v1/cluster/locate/{name}` and, through the combined
  access gate, on the MoQ session (`?jwt=`), where it grants Play on any
  stream without `[auth]` keys or `[[access.rules]]` (an edge enforces those
  itself, for its own viewers).
- **Pull on first viewer, any protocol**: `caudal-core` gets a `Demand` hook
  (`Registry::set_demand`, `Registry::get_or_demand`). LL-HLS, WHEP, RTSP and
  SRT play call `get_or_demand` after `authorize`, so an unauthorized request
  never starts a pull. The edge's `Demand` locates the stream on the origins
  in config order, subscribes to it over MoQ (moq-native client, the same
  pinned `moq-net`/`hang` versions as `caudal-moq`) and publishes it into the
  local registry as an ordinary stream. Every output then serves it unchanged,
  MoQ included (so an edge can be another edge's origin: relay-of-relays
  works without extra code, but is not tested).
- **Sharing**: one pull per stream name per edge, whatever the number of
  viewers or protocols.
- **Idle teardown**: the pull stops `idle_timeout_secs` after the stream's
  viewer count (all outputs) last was non-zero; the grace period also covers
  the first viewer's startup.
- **Origin failover**: when the MoQ session or broadcast from the current
  origin ends, or no frame arrived for 2 s, the edge keeps its local
  `Publisher` (viewers are not restarted), tries the other origins starting
  with the next one, and resumes on the first one that has the stream.
  The 2 s frame watchdog is needed because a killed origin sends no QUIC
  close: without it the edge waited for the QUIC idle timeout (measured
  35 s). Timestamps are rebased so they continue from the last frame the
  edge sent. If no origin has the stream for `source_timeout` (10 s), the
  local stream ends. The `[[failover]]` (backup source) section does not
  exist on `main` yet, so this does not depend on it.
- **Timestamps**: MoQ's Legacy container carries a presentation timestamp
  (microseconds) and no decode timestamp. The edge authors DTS the way
  `moq-mux`'s own FLV export does (`dts = max(prev + 1, pts - reserve)`),
  with the reserve learned from the reordering it sees (B-frames), so DTS is
  monotonic and, after the first reordered frame, never above PTS. Video uses
  a 90 kHz clock and audio its sample rate.
- **Metrics** (edge, `/metrics`): `caudal_cluster_pulls{stream,origin}` (1 per
  active pull), `caudal_cluster_pull_setup_seconds{stream,origin}` (demand or
  failover to first frame), `caudal_cluster_failovers_total{stream}`.
- **Not in scope**: relay-of-relays topologies beyond "an edge's origin list
  may name another edge", origin discovery (the list is static), load-aware
  origin choice, MoQ viewers triggering a pull on an edge (the MoQ session
  only sees streams that are already live there).

## Measured (19 Sep 2026, M-series Mac, debug builds, loopback)

- **Edge hop**, origin ring to edge ring for the same video frame
  (`crates/caudal-cluster/tests/cluster.rs`, `edge_hop_latency`): median
  0.35 ms, p95 0.60 ms, max 0.79 ms over 240 frames.
- **LL-HLS glass-to-glass** with the bench client's `latency-hls` (burned-in
  wall-clock stamp, 1080p30 6 Mb/s, 200 ms parts, 20 samples per run, two
  alternating rounds): origin 539 / 409 ms median, edge 375 / 408 ms. The
  hop is below what LL-HLS part timing can resolve (the run-to-run spread on
  the same node is larger than the hop).
- **First viewer on the edge** (e2e, real binaries, ffmpeg RTMP publisher):
  first playlist answered in ~15 ms, first complete 2 s segment listed
  ~1.3-1.4 s after the first request; pull setup (demand to first frame)
  11 ms.
- **Origin killed** (e2e, `kill -9`-style, the origin carrying the pull; a
  second origin has the same stream from its own ffmpeg): the edge's ingest
  stalls 2.06 s (the watchdog), then continues from origin B; the ffmpeg
  viewer on the edge's LL-HLS decoded 599 of 600 frames of its 20 s run and
  exited cleanly, never restarting.

Code: `crates/caudal-cluster`.
