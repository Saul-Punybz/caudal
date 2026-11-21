//! MPEG-TS in and out for Caudal: `ts` reassembles PES units, `demux` turns
//! them into caudal-core frames (Annex B → AVCC, ADTS → raw AAC), `mux`
//! writes frames back out as a live transport stream. `mpeg` frames the
//! MPEG-2 video and MPEG audio that broadcast TS carries (passed through
//! to TS outputs only). Moved out of
//! `caudal-srt` in batch 7 so the transcoder can talk TS to ffmpeg too.

pub mod demux;
pub mod ingest;
pub mod mpeg;
pub mod mux;
pub mod ts;
