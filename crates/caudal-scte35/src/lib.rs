//! SCTE-35 for Caudal: turns a `splice_info_section` into a
//! [`caudal_core::CueKind`] plus its splice time, and builds sections for
//! cues Caudal generates (API insertion, RTMP `onCuePoint` without a
//! section).
//!
//! Parsing and serializing are done by `scte35-splice` (pinned `=2.1.0`),
//! but none of its types cross this module's API: callers see bytes,
//! [`CueKind`], and 90 kHz tick counts. Swapping the backend (the documented
//! fallback is `scte35-reader`, parse only) touches this file alone.
//!
//! Generated cues default to `time_signal()` + `segmentation_descriptor()`
//! (what SSAI platforms prefer); `splice_insert()` on request.

use broadcast_common::{Parse, Serialize};
use bytes::Bytes;
use caudal_core::CueKind;
use scte35_splice::SpliceInfoSection;
use scte35_splice::commands::{AnyCommand, SpliceInsert, TimeSignal};
use scte35_splice::descriptors::{AnySpliceDescriptor, SegmentationDescriptor, SegmentationTypeId};
use scte35_splice::section::TIER_IGNORE;
use scte35_splice::time::{BreakDuration, SpliceTime};

/// PTS and splice times are 33-bit counts of 90 kHz ticks.
pub const PTS_MASK: u64 = (1 << 33) - 1;
const TICKS_PER_SEC: i64 = 90_000;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("not a valid splice_info_section: {0}")]
    Invalid(String),
    #[error("not hex")]
    NotHex,
}

/// What a section says, in Caudal's terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Splice {
    pub kind: CueKind,
    /// When the splice happens, on the source's 33-bit 90 kHz PTS clock,
    /// with `pts_adjustment` already applied. `None` for an immediate
    /// command (no `splice_time`), which applies where it is received.
    pub pts_90k: Option<u64>,
}

/// Which splice command [`build`] writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Command {
    /// `time_signal()` carrying a `segmentation_descriptor()`.
    #[default]
    TimeSignal,
    /// `splice_insert()` with `out_of_network_indicator` and `break_duration`.
    SpliceInsert,
}

/// Segmentation types that open an ad break (SCTE 35 2023r1 Table 23):
/// Break, Provider/Distributor Advertisement, Placement Opportunity and Ad
/// Block starts. Overlay placement opportunities do not leave the network.
const OUT_TYPES: &[u8] = &[0x22, 0x30, 0x32, 0x34, 0x36, 0x44, 0x46];
/// The matching ends.
const IN_TYPES: &[u8] = &[0x23, 0x31, 0x33, 0x35, 0x37, 0x45, 0x47];

/// 90 kHz ticks to microseconds, rounding down.
pub fn ticks_to_us(ticks: i64) -> i64 {
    (i128::from(ticks) * 1_000_000).div_euclid(i128::from(TICKS_PER_SEC)) as i64
}

/// Microseconds to 90 kHz ticks, rounding up, so that
/// `us_to_ticks(ticks_to_us(t)) == t` for every tick count.
pub fn us_to_ticks(us: i64) -> i64 {
    (i128::from(us) * i128::from(TICKS_PER_SEC) + 999_999).div_euclid(1_000_000) as i64
}

fn invalid(e: impl std::fmt::Display) -> Error {
    Error::Invalid(e.to_string())
}

/// Parses one whole section (table_id `0xFC` through CRC_32, CRC checked).
pub fn parse(section: &[u8]) -> Result<Splice, Error> {
    let s = SpliceInfoSection::parse(section).map_err(invalid)?;
    let Some(clear) = s.clear.as_ref() else {
        // Encrypted: the command is unreadable, but still worth carrying.
        return Ok(Splice { kind: CueKind::Other, pts_90k: None });
    };
    let adjust = |t: Option<u64>| t.map(|p| (p + s.pts_adjustment) & PTS_MASK);
    let splice = match &clear.command {
        AnyCommand::SpliceInsert(i) => {
            let kind = if i.splice_event_cancel_indicator {
                CueKind::Other
            } else if i.out_of_network_indicator {
                CueKind::Out { duration_us: i.break_duration.map(|d| ticks_to_us(d.duration as i64)) }
            } else {
                CueKind::In
            };
            let pts = if i.splice_immediate_flag { None } else { i.splice_time.and_then(|t| t.pts_time) };
            Splice { kind, pts_90k: adjust(pts) }
        }
        AnyCommand::TimeSignal(t) => {
            let mut kind = CueKind::Other;
            for d in s.descriptors() {
                let Ok(AnySpliceDescriptor::Segmentation(seg)) = d else { continue };
                if seg.segmentation_event_cancel_indicator {
                    continue;
                }
                let ty = seg.segmentation_type_id.to_u8();
                if OUT_TYPES.contains(&ty) {
                    let duration_us = seg.segmentation_duration.map(|d| ticks_to_us(d as i64));
                    kind = CueKind::Out { duration_us };
                    break;
                }
                if IN_TYPES.contains(&ty) {
                    kind = CueKind::In;
                    break;
                }
            }
            Splice { kind, pts_90k: adjust(t.splice_time.pts_time) }
        }
        _ => Splice { kind: CueKind::Other, pts_90k: None },
    };
    Ok(splice)
}

/// Builds a section for a cue Caudal generates. `pts_90k` is the splice
/// time on the 33-bit 90 kHz clock (`None`: immediate). `CueKind::Other`
/// always becomes a bare `time_signal()`.
pub fn build(kind: CueKind, pts_90k: Option<u64>, event_id: u32, command: Command) -> Result<Bytes, Error> {
    let splice_time = SpliceTime { pts_time: pts_90k.map(|p| p & PTS_MASK) };
    let mut descriptor_loop = Vec::new();
    let cmd = match (command, kind) {
        (Command::SpliceInsert, CueKind::Out { .. } | CueKind::In) => {
            let duration = match kind {
                CueKind::Out { duration_us: Some(us) } => Some(BreakDuration {
                    auto_return: true,
                    duration: u64::try_from(us_to_ticks(us)).map_err(invalid)?,
                }),
                _ => None,
            };
            AnyCommand::SpliceInsert(SpliceInsert {
                splice_event_id: event_id,
                out_of_network_indicator: matches!(kind, CueKind::Out { .. }),
                splice_immediate_flag: pts_90k.is_none(),
                event_id_compliance_flag: false,
                splice_time: pts_90k.map(|_| splice_time),
                break_duration: duration,
                ..SpliceInsert::default()
            })
        }
        (_, CueKind::Out { .. } | CueKind::In) => {
            let (ty, duration) = match kind {
                CueKind::Out { duration_us } => (
                    SegmentationTypeId::ProviderPlacementOpportunityStart,
                    duration_us.map(|us| u64::try_from(us_to_ticks(us))).transpose().map_err(invalid)?,
                ),
                _ => (SegmentationTypeId::ProviderPlacementOpportunityEnd, None),
            };
            let seg = SegmentationDescriptor {
                segmentation_event_id: event_id,
                segmentation_event_id_compliance_indicator: false,
                segmentation_duration: duration,
                segmentation_type_id: ty,
                segment_num: 1,
                segments_expected: 1,
                sub_segments: ty.has_sub_segments().then_some((0, 0)),
                ..SegmentationDescriptor::default()
            };
            descriptor_loop = vec![0; seg.serialized_len()];
            seg.serialize_into(&mut descriptor_loop).map_err(invalid)?;
            AnyCommand::TimeSignal(TimeSignal { splice_time })
        }
        (_, CueKind::Other) => AnyCommand::TimeSignal(TimeSignal { splice_time }),
    };
    let mut section = SpliceInfoSection::new_clear(cmd, &descriptor_loop);
    section.tier = TIER_IGNORE;
    serialize(&section)
}

fn serialize(section: &SpliceInfoSection<'_>) -> Result<Bytes, Error> {
    let mut out = vec![0; section.serialized_len()];
    section.serialize_into(&mut out).map_err(invalid)?;
    Ok(Bytes::from(out))
}

/// Rewrites `pts_adjustment` so the section's splice time lands on
/// `target_pts_90k`, for muxers whose output clock differs from the
/// source's. A section without a splice time (immediate, encrypted) is
/// returned unchanged; the CRC is recomputed.
pub fn retime(section: &[u8], target_pts_90k: u64) -> Result<Bytes, Error> {
    let mut s = SpliceInfoSection::parse(section).map_err(invalid)?;
    let pts = s.clear.as_ref().and_then(|c| match &c.command {
        AnyCommand::SpliceInsert(i) if !i.splice_immediate_flag => i.splice_time.and_then(|t| t.pts_time),
        AnyCommand::TimeSignal(t) => t.splice_time.pts_time,
        _ => None,
    });
    let Some(pts) = pts else { return Ok(Bytes::copy_from_slice(section)) };
    s.pts_adjustment = target_pts_90k.wrapping_sub(pts) & PTS_MASK;
    serialize(&s)
}

/// `0xFC30...`: the form `EXT-X-DATERANGE` `SCTE35-*` attributes take
/// (RFC 8216 §4.3.2.7.1, a hexadecimal-sequence).
pub fn to_hex(section: &[u8]) -> String {
    format!("0x{}", hex::encode_upper(section))
}

/// Accepts `to_hex` output, with or without the `0x`, in either case.
pub fn from_hex(s: &str) -> Result<Vec<u8>, Error> {
    let s = s.trim();
    let s = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    hex::decode(s).map_err(|_| Error::NotHex)
}

/// A section carried as text (RTMP `onCuePoint` parameters, API bodies):
/// hex (with or without `0x`) or base64. `Some` only if it parses as a
/// `splice_info_section`, CRC included.
pub fn decode_text(s: &str) -> Option<(Bytes, Splice)> {
    use base64::Engine as _;
    let s = s.trim();
    let bytes = from_hex(s).ok().or_else(|| base64::engine::general_purpose::STANDARD.decode(s).ok())?;
    let splice = parse(&bytes).ok()?;
    Some((Bytes::from(bytes), splice))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// splice_insert from the scte35-splice examples (and the SCTE 35
    /// spec's sample 14.2): out of network, event 0x4800008F, PTS
    /// 0x07369C02E, break 0x00052CCF5 with auto return.
    const SPLICE_INSERT: [u8; 50] = [
        0xFC, 0x30, 0x2F, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0xFF, 0xF0, 0x14, 0x05, 0x48, 0x00, 0x00, 0x8F,
        0x7F, 0xEF, 0xFE, 0x73, 0x69, 0xC0, 0x2E, 0xFE, 0x00, 0x52, 0xCC, 0xF5, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0A,
        0x00, 0x08, 0x43, 0x55, 0x45, 0x49, 0x00, 0x00, 0x01, 0x35, 0x62, 0xDB, 0xA3, 0x0A,
    ];

    #[test]
    fn parses_a_known_splice_insert() {
        let s = parse(&SPLICE_INSERT).unwrap();
        assert_eq!(s.pts_90k, Some(0x0_7369_C02E));
        assert_eq!(s.kind, CueKind::Out { duration_us: Some(ticks_to_us(0x0052_CCF5)) });
    }

    #[test]
    fn time_signal_round_trips_to_the_same_kind() {
        for kind in [CueKind::Out { duration_us: Some(30_000_000) }, CueKind::Out { duration_us: None }, CueKind::In] {
            let bytes = build(kind, Some(0x1_2345_6789), 7, Command::TimeSignal).unwrap();
            assert_eq!(bytes[0], 0xFC);
            let back = parse(&bytes).unwrap();
            assert_eq!(back, Splice { kind, pts_90k: Some(0x1_2345_6789) }, "{kind:?}");
        }
    }

    #[test]
    fn splice_insert_round_trips_on_request() {
        let kind = CueKind::Out { duration_us: Some(15_000_000) };
        let bytes = build(kind, Some(900_000), 42, Command::SpliceInsert).unwrap();
        assert_eq!(bytes[13], 0x05, "splice_command_type");
        assert_eq!(parse(&bytes).unwrap(), Splice { kind, pts_90k: Some(900_000) });
        let imm = build(CueKind::In, None, 42, Command::SpliceInsert).unwrap();
        assert_eq!(parse(&imm).unwrap(), Splice { kind: CueKind::In, pts_90k: None });
    }

    #[test]
    fn other_is_a_bare_time_signal() {
        let bytes = build(CueKind::Other, None, 0, Command::SpliceInsert).unwrap();
        assert_eq!(bytes[13], 0x06);
        assert_eq!(parse(&bytes).unwrap(), Splice { kind: CueKind::Other, pts_90k: None });
    }

    #[test]
    fn pts_adjustment_is_applied_and_wraps() {
        let bytes = build(CueKind::In, Some(PTS_MASK - 10), 1, Command::TimeSignal).unwrap();
        // Move the splice 100 ticks later: across the 33-bit wrap.
        let moved = retime(&bytes, 89).unwrap();
        assert_eq!(parse(&moved).unwrap().pts_90k, Some(89));
        // An immediate cue has nothing to move.
        let imm = build(CueKind::In, None, 1, Command::TimeSignal).unwrap();
        assert_eq!(retime(&imm, 5).unwrap(), imm);
    }

    #[test]
    fn corrupt_sections_are_rejected() {
        let mut bad = SPLICE_INSERT;
        bad[20] ^= 0xFF;
        assert!(matches!(parse(&bad), Err(Error::Invalid(_))), "CRC must be checked");
        assert!(parse(&[0xFC]).is_err());
    }

    #[test]
    fn tick_conversions_round_trip() {
        for t in [0, 1, 2, 3, 8, 9, 3000, 90_000, 0x1_FFFF_FFFF, -3000] {
            assert_eq!(us_to_ticks(ticks_to_us(t)), t, "{t}");
        }
        assert_eq!(us_to_ticks(1_000_000), 90_000);
    }

    #[test]
    fn hex_round_trip() {
        let h = to_hex(&SPLICE_INSERT);
        assert!(h.starts_with("0xFC302F"));
        assert_eq!(from_hex(&h).unwrap(), SPLICE_INSERT);
        assert_eq!(from_hex("fc30").unwrap(), vec![0xFC, 0x30]);
        assert_eq!(from_hex("0xZZ"), Err(Error::NotHex));
    }

    #[test]
    fn text_sections_in_hex_or_base64() {
        use base64::Engine as _;
        let (b, s) = decode_text(&to_hex(&SPLICE_INSERT)).unwrap();
        assert_eq!(&b[..], &SPLICE_INSERT);
        assert!(matches!(s.kind, CueKind::Out { .. }));
        let b64 = base64::engine::general_purpose::STANDARD.encode(SPLICE_INSERT);
        assert_eq!(&decode_text(&b64).unwrap().0[..], &SPLICE_INSERT);
        assert!(decode_text("cue-out").is_none());
        assert!(decode_text("").is_none());
    }
}
