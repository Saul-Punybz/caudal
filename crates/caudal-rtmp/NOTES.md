# caudal-rtmp implementation notes

## Library choice

`scuffle-rtmp`'s `ServerSession::new(io, handler)` takes anything
`AsyncRead + AsyncWrite + Unpin`, so it drives happily on our own
`tokio::net::TcpListener` — no need for the `rml_rtmp` fallback. Handshake,
chunking, AMF0 commands and the publish/unpublish/data callback surface
(`SessionHandler`) are entirely handled by scuffle; `caudal-rtmp` only
implements that trait and parses FLV tag payloads.

## Raw-byte fidelity (the reason `scuffle-flv`'s body demuxers aren't used for video)

The contract requires `TrackInfo::init` to be the *raw* avcC/hvcC/av1C bytes
exactly as carried on the wire, and `Frame::data` to be the raw AVCC-framed
NAL data. `scuffle-flv`'s `VideoTagBody::demux` parses AVC/HEVC/AV1 sequence
headers into structured `AVCDecoderConfigurationRecord` /
`HEVCDecoderConfigurationRecord` / `AV1CodecConfigurationRecord` values —
there is no byte-exact re-serialization guarantee.

So `demux.rs` only uses `scuffle_flv::video::header::VideoTagHeader::demux`
(header only, never touches the body), then slices whatever bytes remain in
the tag directly with `Bytes::slice` — zero-copy, byte-exact. The one place
we manually consume bytes past the header is the 3-byte composition-time
field on Enhanced RTMP `CodedFrames` packets for AVC/HEVC (which the FLV
spec defines as part of the body, not the header); AV1 and `CodedFramesX`
carry no composition time.

Audio is different: `scuffle-flv`'s `AacAudioData::SequenceHeader`/`Raw` and
the enhanced `AudioPacket::SequenceStart`/`CodedFrames` variants already hand
back raw, unparsed `Bytes` (verified by reading `scuffle-flv`'s own body
demux source), so `AudioData::demux` (header + body) is used directly there.

Legacy FLV's composition-time field is transmitted as an *unsigned* 24-bit
integer even though real values can be negative once B-frames are involved;
`demux::u24_to_i32` reinterprets it as two's-complement. The Enhanced RTMP
path reads it with `byteorder`'s `read_i24`, which already sign-extends.

## Track announcement timing

`Shared` (in `session.rs`) holds one `Publisher` plus a `Pending` struct
(`video`/`audio`/`meta_fps`/`announced`) behind a `parking_lot::Mutex`.
`try_announce` fires `set_tracks` as soon as both video and audio init
arrive; a spawned task holding only a `Weak<Shared>` fires it after 2s with
whatever showed up (audio-only/video-only streams are valid), and exits
without touching anything if the stream already ended (`Weak::upgrade`
fails) or was already announced by the fast path. Media frames arriving
before announcement are simply pushed and rejected by
`caudal_core::Stream::push` as `PushError::UnknownTrack` — that rejection
*is* the "drop frames before announcement" behavior, no extra bookkeeping
needed.

## Wrong app / busy stream rejection

`scuffle_rtmp::session::server::ServerSessionError` is a closed, fixed enum
(`Timeout`, `PublishBeforeConnect`, `PlayNotSupported`, `InvalidChunkSize`)
with no "other/custom" variant, and `caudal-rtmp` cannot modify a dependency.
So a wrong-app or already-busy publish is rejected at the `caudal-core`
level instead of the RTMP protocol level: `on_publish` returns `Ok(())` (the
handshake completes) but no `Publisher` is created and no entry is added to
`Handler::streams`, so every subsequent video/audio/AMF0 message for that
`stream_id` is silently dropped and nothing ever reaches the `Registry`.
Confirmed by the `wrong_app_is_rejected` test. `PublishError::Busy` is
handled the same way (see `Handler::on_publish`); it isn't covered by its
own integration test (would need two concurrent ffmpeg publishers racing
for the same name) given the batch's time budget, but the code path is
identical to the wrong-app one.

## Video dimensions / fps

Width/height come from the SPS NAL inside the sequence header
(`scuffle_h264::Sps::width()/height()` for AVC, `scuffle_h265::SpsRbsp`'s
`cropped_width()/cropped_height()` for HEVC, `scuffle_av1`'s
`SequenceHeaderObu::max_frame_width/height` for AV1 — all "free" from the
library). Any parse failure defaults to `(0, 0)` rather than dropping the
track: the raw init bytes (what actually matters for playback) are
preserved regardless. `fps` is read from `onMetaData`'s `framerate` key when
present, applied at video-init time or retroactively (re-announcing tracks)
if metadata arrives late.

## Disconnect detection

Not implemented here at all — it's inherent to `scuffle-rtmp`: `drive()`
reads with a 2.5s timeout per attempt, so both a clean TCP close (immediate
EOF) and a dead/killed peer (timeout) end `ServerSession::run()` well within
the 5s budget. `on_unpublish` (from an explicit `deleteStream`) removes the
stream immediately. Either way `Handler::streams` drops its `Arc<Shared>`,
which drops the `Publisher`, which ends the `caudal_core::Stream`.
