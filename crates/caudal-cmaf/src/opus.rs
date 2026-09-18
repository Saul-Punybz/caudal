//! Opus-specific bits that don't belong in `fmp4.rs` or `packager.rs`:
//! parsing the RFC 7845 `OpusHead` that `TrackInfo::init` carries for an
//! Opus track, and reading a packet's duration off its TOC byte (RFC 6716
//! §3.1) instead of assuming a fixed frame size.

use bytes::Bytes;

/// Fields out of an RFC 7845 §5.1 `OpusHead` (little-endian on the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpusHead {
    pub channels: u8,
    pub pre_skip: u16,
    pub input_sample_rate: u32,
    pub output_gain: i16,
}

/// Parses `OpusHead` bytes. `None` if the magic is missing, the packet is
/// short, or the channel mapping needs a mapping table (family != 0) that
/// the `dOps` box we write cannot express.
pub fn parse_opus_head(data: &[u8]) -> Option<OpusHead> {
    if data.len() < 19 || &data[0..8] != b"OpusHead" {
        return None;
    }
    let channels = data[9];
    let pre_skip = u16::from_le_bytes([data[10], data[11]]);
    let input_sample_rate = u32::from_le_bytes([data[12], data[13], data[14], data[15]]);
    let output_gain = i16::from_le_bytes([data[16], data[17]]);
    let mapping_family = data[18];
    if mapping_family != 0 {
        return None;
    }
    Some(OpusHead { channels, pre_skip, input_sample_rate, output_gain })
}

/// Samples (at the packet's 48 kHz clock) covered by one Opus packet, read
/// from its TOC byte (RFC 6716 §3.1) rather than assumed. Every Opus frame
/// duration is one of 2.5/5/10/20/40/60 ms; a packet may bundle several
/// equal-length frames (frame-count codes 1-3).
pub fn frame_duration_samples(packet: &Bytes) -> u32 {
    let Some(&toc) = packet.first() else { return 960 };
    let config = toc >> 3;
    let per_frame = config_frame_samples(config);
    let frames = match toc & 0x03 {
        0 => 1,
        1 | 2 => 2,
        // Code 3: an arbitrary count in the low 6 bits of the next byte.
        _ => packet.get(1).map(|&b| u32::from(b & 0x3f)).filter(|&n| n > 0).unwrap_or(1),
    };
    per_frame.saturating_mul(frames)
}

/// RFC 6716 Table 2: samples per frame at 48 kHz for each of the 32 codec
/// configurations (SILK 10/20/40/60 ms, Hybrid 10/20 ms, CELT 2.5/5/10/20 ms).
fn config_frame_samples(config: u8) -> u32 {
    const SILK: [u32; 4] = [480, 960, 1920, 2880];
    const HYBRID: [u32; 2] = [480, 960];
    const CELT: [u32; 4] = [120, 240, 480, 960];
    match config {
        0..=11 => SILK[(config % 4) as usize],
        12..=15 => HYBRID[(config % 2) as usize],
        _ => CELT[(config % 4) as usize],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(channels: u8, pre_skip: u16, rate: u32, gain: i16, family: u8) -> Vec<u8> {
        let mut h = b"OpusHead".to_vec();
        h.push(1); // version
        h.push(channels);
        h.extend_from_slice(&pre_skip.to_le_bytes());
        h.extend_from_slice(&rate.to_le_bytes());
        h.extend_from_slice(&gain.to_le_bytes());
        h.push(family);
        h
    }

    #[test]
    fn parses_a_stereo_head() {
        let h = head(2, 312, 48_000, 0, 0);
        let parsed = parse_opus_head(&h).unwrap();
        assert_eq!(parsed, OpusHead { channels: 2, pre_skip: 312, input_sample_rate: 48_000, output_gain: 0 });
    }

    #[test]
    fn rejects_short_or_mismatched_magic_or_mapping_table() {
        assert!(parse_opus_head(&[]).is_none());
        assert!(parse_opus_head(b"NotOpusHead........").is_none());
        assert!(parse_opus_head(&head(6, 0, 48_000, 0, 1)).is_none(), "family 1 needs a mapping table");
    }

    #[test]
    fn toc_duration_covers_every_config_family() {
        // SILK NB 10/20/40/60 ms: configs 0-3.
        for (config, samples) in [(0u8, 480u32), (1, 960), (2, 1920), (3, 2880)] {
            assert_eq!(frame_duration_samples(&Bytes::from(vec![config << 3])), samples, "config {config}");
        }
        // Hybrid FB 10/20 ms: configs 14-15.
        assert_eq!(frame_duration_samples(&Bytes::from(vec![14 << 3])), 480);
        assert_eq!(frame_duration_samples(&Bytes::from(vec![15 << 3])), 960);
        // CELT FB 2.5/5/10/20 ms: configs 28-31.
        assert_eq!(frame_duration_samples(&Bytes::from(vec![28 << 3])), 120);
        assert_eq!(frame_duration_samples(&Bytes::from(vec![31 << 3])), 960);
    }

    #[test]
    fn toc_frame_count_codes() {
        // Code 0: one 20 ms frame (CELT WB, config 19).
        let toc0 = 19 << 3;
        assert_eq!(frame_duration_samples(&Bytes::from(vec![toc0])), 960);
        // Code 1: two equal 20 ms frames = 1920 samples.
        let toc1 = (19 << 3) | 1;
        assert_eq!(frame_duration_samples(&Bytes::from(vec![toc1])), 1920);
        // Code 2: two (possibly unequal) 20 ms frames = 1920 samples either way.
        let toc2 = (19 << 3) | 2;
        assert_eq!(frame_duration_samples(&Bytes::from(vec![toc2, 0x00])), 1920);
        // Code 3: arbitrary count, 5 frames of 2.5 ms (config 16) = 600 samples.
        let toc3 = (16 << 3) | 3;
        assert_eq!(frame_duration_samples(&Bytes::from(vec![toc3, 5])), 600);
    }
}
