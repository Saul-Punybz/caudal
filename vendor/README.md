# Vendored crates

## scuffle-rtmp 0.2.3 (MIT OR Apache-2.0), patched

Upstream `ChunkReader` returned the previous header unchanged for every Type 3
chunk. That is right for a continuation chunk, but a Type 3 chunk that starts a
new message must add the last timestamp delta again (RTMP spec 5.3.1.2.4; FFmpeg
`libavformat/rtmppkt.c` does the same). Encoders that send constant-rate frames
as Type 3 (rml_rtmp, which `caudal-restream` uses) got frozen timestamps.

Patch: `src/chunk/reader.rs`, every change marked `Caudal patch`, plus the test
`test_reader_type3_new_messages_reuse_the_delta`. Found 18 Sep 2026 by
`crates/caudal-restream/tests/loopback.rs`. Reported upstream as
https://github.com/ScuffleCloud/scuffle/issues/650; drop this copy once they
ship a fix.

A second patch, same file: `read_chunk` computed how much of a message was
still unread as `new_msg_length - already_buffered_length`. A Type 0/1/2
header always restates the message header, and real streams do restate one
with the same (or a larger) `msg_length` for a message already partway
buffered — that must keep working as a continuation (see
`test_reader_chunk_type0_double_sized`, `test_reader_extended_timestamp`).
But a *smaller* restated `msg_length` (a buggy encoder abandoning a message
mid-stream, or a stream deliberately built to trigger this) underflowed that
subtraction and panicked: a pre-authentication remote crash for any RTMP
publisher, found within the first fuzzing run of
`fuzz/fuzz_targets/rtmp_chunk.rs` (19 Sep 2026, see `fuzz/README.md`). Fixed
by discarding the stale buffered bytes before the subtraction whenever they
would otherwise exceed the new `msg_length`; regression test
`a_smaller_msg_length_on_a_fresh_header_abandons_the_old_partial_message`.
Reported privately upstream on 19 Sep 2026 as GHSA-3gqf-85hw-q8xf (issue #650
above is unrelated).

A third patch, same file, same fuzzing run: the Type 2 branch of
`read_message_header` computed `previous_header.timestamp + timestamp_delta`
with a plain `+` and panicked on overflow. RTMP timestamps are a 32-bit
millisecond counter that legitimately wraps every 49.7 days (this is exactly
why `caudal-rtmp::demux::RtmpClock` exists downstream); the Type 1 branch
right above it already guards the same addition with `checked_add`, Type 2
did not. Fixed with `wrapping_add`, which is also the more correct choice
than Type 1's `checked_add` fallback (reusing the previous timestamp) since
it actually produces the wrapped value instead of silently freezing it —
Type 1's fallback was not touched, since fuzzing did not show it panicking
and it is out of this pass's scope. Regression test
`a_type2_delta_wraps_the_32_bit_timestamp_instead_of_panicking`. Reported privately upstream on 19 Sep 2026 as GHSA-3gqf-85hw-q8xf (ScuffleCloud/scuffle; one report covers all six crashes here).

## scuffle-amf0 0.2.4 (MIT OR Apache-2.0), patched

Found the same day, by the same fuzzing pass, this time in
`fuzz/fuzz_targets/rtmp_flv_amf.rs`. `Amf0Decoder::decode_object`'s
`EcmaArray` branch and `decode_strict_array` both take a 4-byte,
wire-controlled element count and pass it straight to
`with_capacity`/`Vec::with_capacity` before reading a single element. AMF0
`onMetaData`/`onCuePoint` (the only ECMA arrays Caudal ever decodes, from
any RTMP publisher, no auth required first) are exactly this shape, so a
handful of attacker-controlled bytes could make the decoder try to
pre-allocate tens of gigabytes. Worse than the `scuffle-rtmp` panics above:
a failed allocation calls Rust's `handle_alloc_error` and **aborts the
whole process** (not a catchable panic), taking down every stream on the
server, not just the malicious connection.

Patch: `src/decoder.rs`, both call sites marked `Caudal patch`, capping the
capacity *hint* at `MAX_PREALLOC_HINT` (4096) — the loop itself still runs
the full declared count, so a legitimately large array just reallocates a
few extra times; nothing about correct input changes. Regression tests
`a_huge_ecma_array_size_does_not_preallocate_it` and
`a_huge_strict_array_size_does_not_preallocate_it`. Reported privately upstream on 19 Sep 2026 as GHSA-3gqf-85hw-q8xf (ScuffleCloud/scuffle; one report covers all six crashes here).

## scuffle-h264 0.2.2 (MIT OR Apache-2.0), patched

Found the same day, by the same fuzzing pass, one call deeper: once
`rtmp_flv_amf`'s crash in `caudal-rtmp`'s own `demux.rs` (see below) was
fixed, the target kept running and found this. `Sps::width()`/`height()`
compute `base_dimension - crop_offset * 2` with plain `-`/`+`/`*`. Nothing
in `Sps::parse` relates the crop offsets to the frame size, so a
perfectly-parseable SPS (sent by any RTMP/RTSP/SRT publisher, in the
AVCDecoderConfigurationRecord's SPS, before any codec-level validation)
can declare crop offsets far larger than the frame itself: in a debug
build (every `cargo test`/`nextest` run, and this fuzz target, which forces
debug-assertions) the subtraction panics; in a release build (no overflow
checks by default) it silently wraps to a nonsense multi-exabyte
"dimension" instead.

Patch: `src/sps/mod.rs`, both functions marked `Caudal patch`, switched to
saturating arithmetic throughout (every valid SPS's answer is unchanged;
every other input now returns 0 instead of panicking or wrapping).
Regression test `width_and_height_saturate_on_crop_offsets_bigger_than_the_frame`.
Reported privately upstream on 19 Sep 2026 as GHSA-3gqf-85hw-q8xf (ScuffleCloud/scuffle; one report covers all six crashes here).

## scuffle-expgolomb 0.1.5 (MIT OR Apache-2.0), patched

Found the same day, by the same fuzzing pass, one call deeper still: fixing
the `scuffle-h264` bug above let `rtmp_flv_amf` keep running and reach this
one underneath it. `BitReaderExpGolombExt::read_exp_golomb` (used for
*every* Exp-Golomb field in an SPS/PPS -- width, height, crop offsets, and
more) counts leading zero bits with no upper bound and builds its result as
`1 << leading_zeros`. At 64 or more leading zero bits that shift has
already pushed the one bit out of a u64 entirely (`result` is 0), and the
function's final `result - 1` underflowed and panicked. Nothing exotic
about the input needed: 64 zero bits in a row inside any Exp-Golomb field
of an SPS is enough, from any RTMP/RTSP/SRT publisher.

Patch: `src/lib.rs`, marked `Caudal patch`, rejects (`InvalidData`) a
codeNum needing 64+ leading zero bits instead of computing it -- 63 is the
most a u64 can represent (`test_exp_glob_encode`/`test_signed_exp_glob_encode`
round-trip exactly that many, at `u64::MAX - 1`/`i64::MAX`, and still pass).
Regression test
`a_codenum_of_64_or_more_leading_zeros_errors_instead_of_panicking`. Reported privately upstream on 19 Sep 2026 as GHSA-3gqf-85hw-q8xf (ScuffleCloud/scuffle; one report covers all six crashes here).

While making `read_exp_golomb` safe, its sibling `read_signed_exp_golomb`
(same file) got a defense-in-depth pass too: its `as i64` cast plus `-`/`+ 1`
could in principle overflow for a codeNum whose half exceeds `i64::MAX` --
though the `read_exp_golomb` cap above turns out to already rule that case
out (the largest possible codeNum halves to exactly `i64::MAX`), so this is
inert today, not a second live bug. Made saturating anyway in case the cap
ever changes. Regression test
`read_signed_exp_golomb_handles_its_largest_possible_input` (pins the
correct boundary value, not a saturation sentinel, since nothing saturates
today).

Back in `scuffle-h264`: fixing `scuffle-expgolomb` let `rtmp_flv_amf` run
deeper still and find one more, in `src/sps/sps_ext.rs`'s scaling-matrix
loop (`SpsExtended::parse`, reached the same way as the two bugs above it).
`write_signed_exp_golomb`/`read_signed_exp_golomb` can legitimately
round-trip any `i64`, but `next_scale = next_scale + delta_scale + 256`
used plain arithmetic, so a single `delta_scale` near `i64::MAX` or
`i64::MIN` overflowed and panicked on the very first entry of the very
first scaling list -- in an SPS extension whose own doc comment already
says "we don't need [these values] for decoding, so we just skip them".
Fixed with `saturating_add`/`rem_euclid(256)`, which changes nothing about
`next_scale`'s value for any real encoder (whose deltas are always small)
and produces a normal `[0, 256)` value for anything else, matching what
that comment already promised. Regression test
`a_delta_scale_near_i64_max_does_not_overflow_next_scale`. Reported privately upstream on 19 Sep 2026 as GHSA-3gqf-85hw-q8xf (ScuffleCloud/scuffle; one report covers all six crashes here).
