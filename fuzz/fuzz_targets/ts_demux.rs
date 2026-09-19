//! Fuzzes the MPEG-TS ingest pipeline end to end: `caudal_ts::ts::TsDemux`
//! (raw TS packets -> PES/PSI-section elementary-stream units, including
//! the SCTE-35 section reassembly) feeding straight into
//! `caudal_ts::demux::Demuxer` (Annex B -> AVCC/hvcC, ADTS -> raw AAC,
//! `caudal-scte35` cue placement). This is what an SRT publisher's bytes go
//! through (`caudal-srt` hands raw payload to `TsDemux::feed`), so it is
//! reachable pre-authentication on any open SRT listener.
//!
//! `TsDemux` only turns on the SCTE-35 section path once a PMT declares
//! stream_type `0x86` for a PID, so a good seed corpus needs at least one
//! real capture with an SCTE-35 PID (see `fuzz/README.md`) for that branch
//! to be reachable at all.

#![no_main]

use caudal_ts::demux::Demuxer;
use caudal_ts::ts::TsDemux;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut ts = TsDemux::new();
    ts.feed(data);
    let mut units = Vec::new();
    ts.drain(&mut units);
    ts.flush(&mut units);

    let mut demuxer = Demuxer::new();
    let mut events = Vec::new();
    for unit in units {
        demuxer.consume(unit, &mut events);
    }
});
