//! Byte-level helpers: Annex B <-> AVCC, the avcC record, SPS dimensions,
//! the OpusHead, and RTP timestamp unwrapping.

use mp4_atom::{Atom, Avcc};

pub(crate) const NAL_IDR: u8 = 5;
pub(crate) const NAL_SPS: u8 = 7;
pub(crate) const NAL_PPS: u8 = 8;
pub(crate) const NAL_AUD: u8 = 9;

pub(crate) fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |b| b & 0x1f)
}

/// Splits Annex B bytes into NAL units without start codes. Accepts 3- and
/// 4-byte start codes; trailing zero bytes (the leading zero of a 4-byte
/// start code, or `trailing_zero_8bits`) are trimmed, which is safe because
/// a NAL unit never ends in `0x00`.
pub(crate) fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push((i, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::with_capacity(starts.len());
    for (k, &(_, begin)) in starts.iter().enumerate() {
        let mut end = starts.get(k + 1).map_or(data.len(), |&(code, _)| code);
        while end > begin && data[end - 1] == 0 {
            end -= 1;
        }
        if begin < end {
            nals.push(&data[begin..end]);
        }
    }
    nals
}

/// NAL units of an AVCC (4-byte length-prefixed) access unit. Stops at the
/// first malformed length instead of failing.
pub(crate) fn avcc_nals(mut data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    while data.len() >= 4 {
        let len = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
        let Some(nal) = data.get(4..4 + len) else { break };
        if !nal.is_empty() {
            out.push(nal);
        }
        data = &data[4 + len..];
    }
    out
}

pub(crate) fn push_avcc_nal(out: &mut Vec<u8>, nal: &[u8]) {
    out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
    out.extend_from_slice(nal);
}

pub(crate) fn push_annexb_nal(out: &mut Vec<u8>, nal: &[u8]) {
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(nal);
}

/// The avcC body (AVCDecoderConfigurationRecord) for one SPS and one PPS.
pub(crate) fn build_avcc(sps: &[u8], pps: &[u8]) -> Option<Vec<u8>> {
    let avcc = Avcc::new(sps, pps).ok()?;
    let mut out = Vec::new();
    avcc.encode_body(&mut out).ok()?;
    Some(out)
}

/// SPS and PPS NAL units from an avcC body. Lenient: returns what it could
/// read before the record ended.
pub(crate) fn avcc_parameter_sets(rec: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let Some(&n_sps) = rec.get(5) else { return out };
    let mut pos = 6;
    let read = |pos: &mut usize| -> Option<Vec<u8>> {
        let len = u16::from_be_bytes([*rec.get(*pos)?, *rec.get(*pos + 1)?]) as usize;
        let nal = rec.get(*pos + 2..*pos + 2 + len)?.to_vec();
        *pos += 2 + len;
        Some(nal)
    };
    for _ in 0..(n_sps & 0x1f) {
        match read(&mut pos) {
            Some(n) => out.push(n),
            None => return out,
        }
    }
    let Some(&n_pps) = rec.get(pos) else { return out };
    pos += 1;
    for _ in 0..n_pps {
        match read(&mut pos) {
            Some(n) => out.push(n),
            None => return out,
        }
    }
    out
}

/// Width, height and frame rate (if the VUI declares it) from an SPS NAL.
pub(crate) fn sps_params(sps: &[u8]) -> Option<(u32, u32, Option<f64>)> {
    let parsed = scuffle_h264::Sps::parse_with_emulation_prevention(std::io::Cursor::new(sps)).ok()?;
    let fps = parsed.frame_rate().filter(|f| f.is_finite() && *f > 0.0);
    Some((parsed.width() as u32, parsed.height() as u32, fps))
}

/// OpusHead, RFC 7845 §5.1: version 1, pre-skip 312, 48 kHz input, gain 0,
/// channel mapping family 0.
pub(crate) fn opus_head(channels: u8) -> Vec<u8> {
    let mut h = Vec::with_capacity(19);
    h.extend_from_slice(b"OpusHead");
    h.push(1);
    h.push(channels);
    h.extend_from_slice(&312u16.to_le_bytes());
    h.extend_from_slice(&48_000u32.to_le_bytes());
    h.extend_from_slice(&0i16.to_le_bytes());
    h.push(0);
    h
}

/// Turns 32-bit RTP timestamps into a monotonic 64-bit clock starting at
/// `offset` ticks.
#[derive(Debug)]
pub(crate) struct RtpClock {
    last_low: u32,
    ext: i64,
    offset: i64,
    last_out: Option<i64>,
}

impl RtpClock {
    pub(crate) fn new(first: u32, offset: i64) -> Self {
        Self { last_low: first, ext: 0, offset, last_out: None }
    }

    /// Ticks since the first timestamp plus the offset. Never goes back:
    /// a reordered or repeated timestamp comes out one tick after the last.
    pub(crate) fn next(&mut self, ts: u32) -> i64 {
        let delta = i64::from(ts.wrapping_sub(self.last_low) as i32);
        self.last_low = ts;
        self.ext += delta;
        let mut out = self.offset + self.ext;
        if let Some(last) = self.last_out
            && out <= last
        {
            out = last + 1;
        }
        self.last_out = Some(out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annexb_split_trims_and_accepts_both_start_codes() {
        let data = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 0, 1, 0x65, 4, 5];
        let nals = split_annexb(&data);
        assert_eq!(nals, vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 4, 5][..]]);
    }

    #[test]
    fn avcc_roundtrip_and_malformed() {
        let mut out = Vec::new();
        push_avcc_nal(&mut out, &[0x65, 1]);
        push_avcc_nal(&mut out, &[0x41]);
        assert_eq!(avcc_nals(&out), vec![&[0x65, 1][..], &[0x41][..]]);
        out.extend_from_slice(&[0, 0, 9, 9, 1]);
        assert_eq!(avcc_nals(&out).len(), 2);
        assert!(avcc_nals(&[0xff; 3]).is_empty());
    }

    #[test]
    fn rtp_clock_unwraps() {
        let mut c = RtpClock::new(u32::MAX - 1000, 5);
        assert_eq!(c.next(u32::MAX - 1000), 5);
        assert_eq!(c.next(u32::MAX - 1000), 6); // repeated: bumped by one
        assert_eq!(c.next(u32::MAX), 1005);
        assert_eq!(c.next(2000), 1005 + 2001); // across the 32-bit wrap
    }

    #[test]
    fn opus_head_layout() {
        let h = opus_head(2);
        assert_eq!(h.len(), 19);
        assert_eq!(&h[..8], b"OpusHead");
        assert_eq!(h[8], 1);
        assert_eq!(h[9], 2);
        assert_eq!(u16::from_le_bytes([h[10], h[11]]), 312);
        assert_eq!(u32::from_le_bytes([h[12], h[13], h[14], h[15]]), 48_000);
    }

    #[test]
    fn avcc_parameter_sets_are_lenient() {
        assert!(avcc_parameter_sets(&[]).is_empty());
        let rec = [1, 0x42, 0xc0, 0x1f, 0xff, 0xe1, 0, 2, 0x67, 0x42, 1, 0, 1, 0x68];
        assert_eq!(avcc_parameter_sets(&rec), vec![vec![0x67, 0x42], vec![0x68]]);
        assert_eq!(avcc_parameter_sets(&rec[..9]), Vec::<Vec<u8>>::new());
    }
}
