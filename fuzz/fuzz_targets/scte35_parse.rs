//! Fuzzes `caudal-scte35`'s public API: `parse` (a raw `splice_info_section`,
//! CRC checked, as carried in an MPEG-TS PID or an RTMP `onCuePoint`
//! parameter), `decode_text` (the hex/base64 text form from RTMP or an API
//! body) and `retime` (rewrites `pts_adjustment` on an already-parsed
//! section). Reachable from three untrusted inputs: an SRT/TS publisher's
//! SCTE-35 PID (via `ts_demux`), an RTMP `onCuePoint` message (via
//! `rtmp_flv_amf`), and any HTTP API that accepts a cue by text.
//!
//! Input layout: `[op: u8][rest]`. `op % 3` selects `parse` (rest is the
//! raw section), `decode_text` (rest as UTF-8 hex/base64 text), or `retime`
//! (`rest[..8]` little-endian `target_pts_90k`, `rest[8..]` the section).
//! A real `splice_info_section` (the crate's own test vector, or one from
//! `threefive`/a broadcast capture) dropped straight into `op=0` byte plus
//! bytes is a perfectly good corpus entry.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&op, rest)) = data.split_first() else { return };
    match op % 3 {
        0 => {
            let _ = caudal_scte35::parse(rest);
        }
        1 => {
            if let Ok(s) = std::str::from_utf8(rest) {
                let _ = caudal_scte35::decode_text(s);
            }
        }
        _ => {
            if rest.len() >= 8 {
                let (pts_bytes, section) = rest.split_at(8);
                let target_pts_90k = u64::from_le_bytes(pts_bytes.try_into().unwrap());
                let _ = caudal_scte35::retime(section, target_pts_90k);
            }
        }
    }
});
