//! CMAF / fragmented MP4 writing, shared by the LL-HLS packager and the
//! recorder: init segments (`ftyp` + `moov`) for H.264, H.265, AAC and Opus,
//! and `moof` + `mdat` fragments. Moved out of `caudal-hls` in batch 6.

pub mod fmp4;
pub mod opus;
