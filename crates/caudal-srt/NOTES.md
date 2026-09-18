# caudal-srt: reuse decisions and design notes

## SRT transport: `rsrt`

`rsrt` 0.3.6 is pure-Rust, tokio-based, live mode only (no file/messaging,
no rendezvous). One `SrtListener` accepts both publishers and viewers; a
caller connects with `SrtSocket::connect`. The same `SrtSocket::send`/`recv`
API works whichever side established the connection, which is what lets
`connection.rs` (ingest, listener-accepted) and `play.rs`/`push.rs` (egress,
listener-accepted and caller-connected respectively) share so much shape.
See `REUSE.md` for the license/version/verification trail.

## TS demux (ingest, batch 2): `mpeg2ts` for parsing

`crate::ts::TsDemux` reads TS/PSI/PES with `mpeg2ts::ts::{TsPacketReader,
ReadTsPacket}`, feeding a `SharedQueue` (`impl Read`) so partial SRT
payloads never look like EOF to the reader. `crate::demux::Demuxer` turns
reassembled PES payloads into `caudal_core::Frame`s: Annex-B -> AVCC,
avcC/hvcC built once per SPS/PPS/VPS change, ADTS -> `AudioSpecificConfig`.
Emulation-prevention/start-code edge case: a NAL immediately preceded by a
4-byte start code may keep one extra leading `0x00` from the previous NAL's
trailing padding when `split_annexb` walks 3-byte codes; every decoder this
project has tested against (x264, ffmpeg, hardware decoders via `ffprobe`/
`ffmpeg -f null -`) tolerates a leading zero byte, so this is left as is.
Resync caveat: a single malformed TS packet is logged and skipped, which
can desynchronize continuity-counter-based loss detection for the rest of
that connection; acceptable since a malformed publisher is rare and only
ever affects its own connection (`serve`'s per-connection task model).

## TS mux (egress, batch 6): hand-rolled on `mpeg2ts::ts::TsPacketWriter`, not moq-mux

**Decision, made in the time-boxed reuse pass**: `moq-mux` 0.9.16's
`container/ts/export.rs` cannot be driven from raw frames. Its `Export::new`
takes a `crate::Source`, which is `{ moq_net::origin::Consumer, path }` —
`Export` subscribes to a **hang broadcast** through a `moq_net::Origin` and
reads a hang **catalog** to learn tracks; there is no entry point that
accepts a bare `Frame`/`TrackInfo` stream. Using it here would mean standing
up an in-process `moq_net::Origin` and publishing a hang broadcast per SRT
viewer/push just to immediately re-consume it for TS export — the same
machinery `caudal-moq` (batch 5) already runs for actual MoQ output, not a
reuse win for a second output. So: **`mpeg2ts::ts::TsPacketWriter` directly**
(the same crate ingest already depends on, which writes as well as reads —
confirmed via `crates.io`/its own test suite before committing to this).
Everything above `TsPacketWriter` (PES chunking, PAT/PMT, PCR, AVCC->Annex-B,
ADTS) is this crate's own code, in `src/mux.rs`.

### What's hand-written and why

- **`TsPacketWriter`/`TsPacket`/`TsHeader` write path**: reused verbatim.
- **PES chunking across 188-byte packets**: `mpeg2ts`'s `ts::payload::Pes`
  only carries *one packet's worth* of ES data (mirrors how `crate::ts`
  reads it back: `PesStart` + zero or more `PesContinuation`), so
  `TsMux::write_pes` does the chunking loop itself, sizing each chunk to
  `184 - (PES header, first packet only) - (adaptation field, first packet
  only)`. `mpeg2ts::TsPacket::write_to` pads whatever is left in a packet
  with stuffing automatically (bare adaptation field when none was
  requested), so slices don't need to fill a packet exactly.
- **`AdaptationField::external_size`** is `pub(super)` inside `mpeg2ts`
  (private to that crate), so `mux::adaptation_field_size` mirrors it for
  the one shape this muxer ever builds: length + flags bytes, plus 6 when a
  PCR is present. `PesHeader::optional_header_len` is likewise private;
  `mux::pes_optional_header_len` mirrors it exactly like `crate::ts`'s
  demux-side copy already does (same ISO/IEC 13818-1 2.4.3.7 formula).
- **AVCC -> Annex B, SPS/PPS/VPS re-injection**: `crate::ts`'s ingest demux
  already strips parameter sets and ADUs from `Frame.data` (AVCC-framed VCL
  NALs only) and puts SPS/PPS/VPS in the track's `avcC`/`hvcC` init record.
  `mux::VideoTrack::{from_avcc,from_hvcc}` decode that record back
  (`mp4_atom::Avcc`/`Hvcc::decode_body` — the same crate already used to
  *build* it on ingest, note `mp4_atom`'s `Buf` is its own trait, not
  `bytes::Buf`, so decoding goes through `&[u8]`, not `bytes::Bytes`
  directly). SPS/PPS/VPS are re-emitted before every keyframe (matches the
  batch brief); an AUD is emitted before every access unit, hardcoded as
  the common two encoders (x264/x265 `aud=1`) emit it (H.264 `[0x09, 0xF0]`,
  H.265 `[0x46, 0x01, 0x50]`) since AUDs are informative/discardable and no
  decoder in this project's test matrix rejects an approximate `pic_type`.
- **ADTS**: `mux::AudioTrack::from_asc` reverses
  `crate::ts::AdtsHeader::audio_specific_config` exactly (both sides agree
  on the 2-byte, no-extension `AudioSpecificConfig` layout), so the
  round-trip through ADTS -> `AudioSpecificConfig` (ingest) -> ADTS
  (egress) is lossless for the object types this project emits (LC and
  friends within the 2-bit ADTS "profile" field's range).
- **Opus**: no attempt to carry it in TS (there is no standard MPEG-2 TS
  mapping this project's dependencies give us "for free", and hand-rolling
  one — a registration descriptor plus an Opus-in-TS framing convention —
  was judged out of scope for this batch). `TsMux::set_tracks` logs once
  per stream (`warned_opus`) and drops Opus audio frames; video (and any
  AAC audio on the same stream) still plays.
- **PCR/PSI cadence**: PSI (PAT/PMT) at start, on every video keyframe, and
  at least every 100 ms (`PSI_INTERVAL`); PCR on the video PID (or the
  audio PID for an audio-only stream) at least every 40 ms
  (`PCR_INTERVAL`, matching TR 101 290's 40 ms gap flag), both gated by
  wall-clock `Instant`s (this is a live pass-through mux, so wall time and
  media time track closely enough) but the **PCR value itself** is derived
  from that video frame's DTS (`to_90k(frame.dts, ...) * 300`), not wall
  time, so it stays correct relative to the bitstream regardless of any
  jitter in when this process happens to run.
- **PTS/DTS scale**: `mux::to_90k` converts any track's native timescale to
  the 90 kHz, 33-bit-wrapped clock TS requires. Video is already 90 kHz
  (identity conversion, no rounding); audio (its own sample rate, per
  `crate::ts`'s ingest-side comment on why frames stay in native scale) is
  rescaled per frame.

### A real bug this caught: unbounded video PES and stream end

First implementation left video PES length as `0` ("unbounded" — the norm
for video, and how `crate::ts`'s own ingest-side reader treats it: "the
next `PesStart` on this PID ends the packet"). That is fine mid-stream, but
the **very last** access unit of a stream (or of a viewer's capture window)
has no following `PesStart` to close it. A synthetic test that mux'd real
H.264 access units (extracted from `caudal-hls/tests/fixtures/av.mp4`, see
`mux::tests::real_h264_fixture_round_trips_through_ffmpeg`) straight through
`ffmpeg -f null -` (no SRT, no truncation) reproduced a "non monotonically
increasing dts" warning right at the tail. Fix: video PES now carries a
**bounded** `pes_packet_len` (`optional_header_len + payload.len()`)
whenever it fits a `u16` (it always does for one H.264/H.265 access unit in
practice), falling back to `0` only if it doesn't. That test is also what
caught an unrelated test-harness mistake on the way: hand-feeding
`pts == dts` for every access unit is wrong for a source with real B-frames
(this fixture has some) — `ffmpeg`'s own remux of the same file was used as
the ground truth for per-access-unit PTS/DTS in that test, once the
fixture's actual frame structure was checked with `ffprobe -show_packets`.

### Test-harness note: `srt-live-transmit` never signals a clean end

Confirmed empirically while writing the pull/push tests: when
`srt-live-transmit` is the **caller** relaying `ffmpeg`'s stdout (the
publish pipeline every ingest test already uses), its stdin hitting EOF
(`ffmpeg` finishing a finite `-t N` encode) does **not** make it send an SRT
shutdown or otherwise close the connection — it just stops sending data and
busy-loops (the same behavior the module doc already calls out for the
receive side). So Caudal's own `data_idle_timeout` (3 s, see `lib.rs`) is
what eventually ends the stream, not a graceful peer close, and a test that
waits for `registry.get(name).is_none()` after a finite-duration source
still has to budget for that idle timeout *plus* however long the pipeline
takes to actually stop sending. The pull/push integration tests instead use
a fixed real-time capture window and hard-kill both ends (the documented
`Pipeline`/`Guarded` process-group pattern), which can truncate the last
video access unit mid-frame — expected for a live-capture cutoff, not a
muxer defect, so those two tests assert `ffmpeg`'s decode **exits
successfully** rather than requiring empty stderr (a trailing "non
monotonically increasing dts" from the truncated tail is tolerated; the
from-a-fixture unit test above is the one that must be byte-clean, since
nothing there is truncated).

## Push reconnect (`push.rs`)

One task per `SrtPush`, spawned from `serve`. `wait_for_publish` blocks on
`Registry::subscribe_publishes()` (never busy-polls) until the configured
stream name (re)appears; `send_until_ended` then connects as a caller and
mux/pumps frames until `Event::End` or a send failure, backing off
1 s -> 30 s (doubling, capped) between reconnect attempts, reset back to
1 s once a session actually sends at least one chunk (so a destination
that accepts the connection but immediately drops it doesn't get hammered
at 1 s forever). Subscribes with `subscribe_internal` (not counted as a
viewer): a push target is an outbound relay, not this server's own
audience, mirroring how the HLS packager is also not counted as a viewer
of its own internal subscription.

## Pull (`play.rs`)

`play/<name>` / `#!::r=<name>,m=request`, parsed by
`connection::parse_play` (mirrors `parse_publish`, `m=request` instead of
`m=publish`). Subscribes with `StartAt::LiveEdge` via `Stream::subscribe`
(counted — "a real viewer"). `Event::Lagged` needs no special handling
beyond a log line: `TsMux` already re-injects SPS/PPS/VPS on every
keyframe regardless of what came before, so the next video frame after a
lag is itself a valid resync point without any extra muxer state.
