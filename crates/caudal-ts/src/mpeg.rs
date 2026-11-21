//! MPEG-2 video (ISO/IEC 13818-2) and MPEG-1/2 audio (ISO/IEC 11172-3,
//! 13818-3) header parsing: just enough to describe a track and to frame
//! it, never to decode it. Broadcast TS (an ATSC chain fed by VLC or a
//! hardware encoder) carries these; Caudal passes them through to its TS
//! outputs untouched.

/// `00 00 01 B3`: sequence header.
const SEQUENCE_HEADER: u8 = 0xB3;
/// `00 00 01 B5`: extension (sequence, display, ... ; id in the next nibble).
const EXTENSION: u8 = 0xB5;
/// `00 00 01 00`: picture header.
const PICTURE: u8 = 0x00;

/// What a sequence header says about the picture.
#[derive(Debug, Clone, PartialEq)]
pub struct Mpeg2Sequence {
    pub width: u32,
    pub height: u32,
    /// From `frame_rate_code` (Table 6-4); `None` for a reserved code.
    pub fps: Option<f64>,
    /// The sequence header and the extensions right after it (sequence
    /// extension, display extension), start codes included: what a decoder
    /// needs before the first picture of a GOP.
    pub header: Vec<u8>,
}

/// Offsets of every `00 00 01 xx` start code in `data` (pointing at the
/// first `00`), with the code byte `xx`.
fn start_codes(data: &[u8]) -> impl Iterator<Item = (usize, u8)> + '_ {
    let mut i = 0usize;
    std::iter::from_fn(move || {
        while i + 3 < data.len() {
            if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                let at = i;
                i += 3;
                return Some((at, data[at + 3]));
            }
            i += 1;
        }
        None
    })
}

/// Frame rates by `frame_rate_code` (ISO/IEC 13818-2 Table 6-4).
fn frame_rate(code: u8) -> Option<f64> {
    Some(match code {
        1 => 24_000.0 / 1001.0,
        2 => 24.0,
        3 => 25.0,
        4 => 30_000.0 / 1001.0,
        5 => 30.0,
        6 => 50.0,
        7 => 60_000.0 / 1001.0,
        8 => 60.0,
        _ => return None,
    })
}

/// The first sequence header in an access unit, if it carries one.
pub fn mpeg2_sequence(data: &[u8]) -> Option<Mpeg2Sequence> {
    let mut codes = start_codes(data).skip_while(|&(_, c)| c != SEQUENCE_HEADER);
    let (begin, _) = codes.next()?;
    // Header fields: 12-bit width, 12-bit height, 4-bit aspect, 4-bit rate.
    let f = data.get(begin + 4..begin + 8)?;
    let mut width = (u32::from(f[0]) << 4) | (u32::from(f[1]) >> 4);
    let mut height = (u32::from(f[1] & 0x0F) << 8) | u32::from(f[2]);
    let fps = frame_rate(f[3] & 0x0F);
    // The header runs to the first start code that is not one of its
    // extensions (a GOP header, a picture, user data).
    let mut end = data.len();
    for (at, code) in codes {
        if code != EXTENSION {
            end = at;
            break;
        }
        // Sequence extension (id 1): 2-bit size extensions above 4095.
        if let Some(e) = data.get(at + 4..at + 7)
            && e[0] >> 4 == 1
        {
            width |= u32::from(((e[1] & 0x01) << 1) | (e[2] >> 7)) << 12;
            height |= u32::from((e[2] >> 5) & 0x03) << 12;
        }
    }
    Some(Mpeg2Sequence { width, height, fps, header: data[begin..end].to_vec() })
}

/// Whether the first picture in an access unit is an I picture
/// (`picture_coding_type` 1): where a viewer can start decoding.
pub fn mpeg2_is_intra(data: &[u8]) -> bool {
    start_codes(data)
        .find(|&(_, c)| c == PICTURE)
        .and_then(|(at, _)| data.get(at + 5))
        .is_some_and(|b| (b >> 3) & 0x07 == 1)
}

/// Whether an access unit already starts its GOP with a sequence header.
pub fn mpeg2_has_sequence_header(data: &[u8]) -> bool {
    start_codes(data).any(|(_, c)| c == SEQUENCE_HEADER)
}

/// MPEG audio version, from the header's 2-bit ID field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MpegVersion {
    /// MPEG-1 (32, 44.1, 48 kHz): TS stream_type 0x03.
    Mpeg1,
    /// MPEG-2 low sample rates (16, 22.05, 24 kHz): stream_type 0x04.
    Mpeg2,
    /// The unofficial MPEG-2.5 extension (8, 11.025, 12 kHz): 0x04 too.
    Mpeg25,
}

/// One MPEG audio frame header (ISO/IEC 11172-3 2.4.1.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MpegAudioHeader {
    pub version: MpegVersion,
    /// 1, 2 or 3.
    pub layer: u8,
    pub sample_rate: u32,
    pub channels: u8,
    /// Whole frame, header included.
    pub frame_len: usize,
    /// PCM samples per channel the frame decodes to.
    pub samples: u32,
}

/// Kilobits per second by bitrate index; 0 is "free format" (unsupported).
const BITRATE_V1_L2: [u32; 15] = [0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384];
const BITRATE_V1_L3: [u32; 15] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
/// MPEG-2 and 2.5, Layers II and III share one table.
const BITRATE_V2_L23: [u32; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];

impl MpegAudioHeader {
    /// Parses a header at the start of `data`. Layer I and free-format
    /// frames (no fixed length) are rejected: nothing in a broadcast chain
    /// sends them, and without a length the frame cannot be split.
    pub fn parse(data: &[u8]) -> Option<Self> {
        let h = data.get(..4)?;
        if h[0] != 0xFF || h[1] & 0xE0 != 0xE0 {
            return None;
        }
        let version = match (h[1] >> 3) & 0x03 {
            0 => MpegVersion::Mpeg25,
            2 => MpegVersion::Mpeg2,
            3 => MpegVersion::Mpeg1,
            _ => return None,
        };
        let layer = match (h[1] >> 1) & 0x03 {
            1 => 3,
            2 => 2,
            _ => return None, // 3 = Layer I, 0 = reserved
        };
        let bitrate_index = usize::from(h[2] >> 4);
        let table = match (version, layer) {
            (MpegVersion::Mpeg1, 2) => &BITRATE_V1_L2,
            (MpegVersion::Mpeg1, _) => &BITRATE_V1_L3,
            _ => &BITRATE_V2_L23,
        };
        let kbps = *table.get(bitrate_index)?;
        if kbps == 0 {
            return None;
        }
        let base_rate = match (h[2] >> 2) & 0x03 {
            0 => 44_100,
            1 => 48_000,
            2 => 32_000,
            _ => return None,
        };
        let sample_rate = match version {
            MpegVersion::Mpeg1 => base_rate,
            MpegVersion::Mpeg2 => base_rate / 2,
            MpegVersion::Mpeg25 => base_rate / 4,
        };
        let padding = usize::from((h[2] >> 1) & 0x01);
        let channels = if h[3] >> 6 == 3 { 1 } else { 2 };
        // Layer III outside MPEG-1 has half the samples per frame.
        let samples = if layer == 3 && version != MpegVersion::Mpeg1 { 576 } else { 1152 };
        let frame_len = (samples as usize / 8) * kbps as usize * 1000 / sample_rate as usize + padding;
        Some(Self { version, layer, sample_rate, channels, frame_len, samples })
    }

    /// The header with every field that may change frame to frame zeroed
    /// (bitrate, padding, private, mode extension, copyright, original,
    /// emphasis): equal for every frame of one encoder's stream, so a change
    /// means a new format.
    pub fn stable_bytes(data: &[u8]) -> [u8; 4] {
        [data[0], data[1], data[2] & 0x0C, data[3] & 0xC0]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1280x720, 59.94 fps sequence header + sequence extension, then a
    /// GOP header and an I picture, as `ffmpeg -c:v mpeg2video` writes them.
    fn i_picture_au() -> Vec<u8> {
        let mut au = vec![0, 0, 1, 0xB3, 0x50, 0x02, 0xD0, 0x37, 0xFF, 0xFF, 0xE0, 0x18];
        au.extend_from_slice(&[0, 0, 1, 0xB5, 0x14, 0x8A, 0x00, 0x01, 0x00, 0x00]);
        au.extend_from_slice(&[0, 0, 1, 0xB8, 0x00, 0x08, 0x00, 0x00]);
        // temporal_reference 0, picture_coding_type 1 (I).
        au.extend_from_slice(&[0, 0, 1, 0x00, 0x00, 0x0F, 0xFF, 0xF8]);
        au.extend_from_slice(&[0, 0, 1, 0x01, 0x12, 0x34]);
        au
    }

    #[test]
    fn sequence_header_gives_size_rate_and_the_header_bytes() {
        let au = i_picture_au();
        let seq = mpeg2_sequence(&au).unwrap();
        assert_eq!((seq.width, seq.height), (1280, 720));
        assert!((seq.fps.unwrap() - 59.94).abs() < 0.01);
        // Sequence header + sequence extension, up to the GOP header.
        assert_eq!(seq.header, au[..22]);
        assert!(mpeg2_is_intra(&au));
        assert!(mpeg2_has_sequence_header(&au));
    }

    #[test]
    fn p_picture_without_a_sequence_header() {
        // picture_coding_type 2 (P): byte 5 after the start code = 0b0001_0xxx.
        let au = [0, 0, 1, 0x00, 0x00, 0x51, 0xFF, 0xF8, 0, 0, 1, 0x01, 0x00];
        assert!(!mpeg2_is_intra(&au));
        assert!(mpeg2_sequence(&au).is_none());
        assert!(!mpeg2_has_sequence_header(&au));
    }

    #[test]
    fn mp2_and_mp3_headers() {
        // MPEG-1 Layer II, 192 kbit/s, 48 kHz, stereo: 576 bytes.
        let mp2 = MpegAudioHeader::parse(&[0xFF, 0xFD, 0xA4, 0x00]).unwrap();
        assert_eq!((mp2.version, mp2.layer, mp2.sample_rate, mp2.channels), (MpegVersion::Mpeg1, 2, 48_000, 2));
        assert_eq!((mp2.frame_len, mp2.samples), (576, 1152));
        // MPEG-1 Layer III, 128 kbit/s, 44.1 kHz, padded, mono: 418 bytes.
        let mp3 = MpegAudioHeader::parse(&[0xFF, 0xFB, 0x92, 0xC0]).unwrap();
        assert_eq!((mp3.layer, mp3.sample_rate, mp3.channels, mp3.frame_len), (3, 44_100, 1, 418));
        // MPEG-2 Layer III, 64 kbit/s, 24 kHz: 576 samples, 192 bytes.
        let lsf = MpegAudioHeader::parse(&[0xFF, 0xF3, 0x84, 0x00]).unwrap();
        assert_eq!((lsf.version, lsf.sample_rate, lsf.samples, lsf.frame_len), (MpegVersion::Mpeg2, 24_000, 576, 192));
    }

    #[test]
    fn rejects_layer_one_free_format_and_garbage() {
        assert!(MpegAudioHeader::parse(&[0xFF, 0xFF, 0xA4, 0x00]).is_none(), "Layer I");
        assert!(MpegAudioHeader::parse(&[0xFF, 0xFD, 0x04, 0x00]).is_none(), "free format");
        assert!(MpegAudioHeader::parse(&[0xFF, 0xFD, 0xF4, 0x00]).is_none(), "bad bitrate");
        assert!(MpegAudioHeader::parse(&[0xFF, 0xFD, 0xAC, 0x00]).is_none(), "reserved rate");
        assert!(MpegAudioHeader::parse(&[0x47, 0x40, 0x11, 0x10]).is_none());
        assert!(MpegAudioHeader::parse(&[0xFF, 0xFD]).is_none());
    }

    #[test]
    fn stable_bytes_ignore_per_frame_fields() {
        // Same format, different bitrate, padding and mode extension.
        let a = MpegAudioHeader::stable_bytes(&[0xFF, 0xFD, 0xA4, 0x00]);
        let b = MpegAudioHeader::stable_bytes(&[0xFF, 0xFD, 0x86, 0x30]);
        assert_eq!(a, b);
        // Mono is a different format.
        assert_ne!(a, MpegAudioHeader::stable_bytes(&[0xFF, 0xFD, 0xA4, 0xC0]));
    }
}
