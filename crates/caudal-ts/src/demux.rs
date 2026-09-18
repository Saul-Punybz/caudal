//! Turns reassembled elementary-stream access units (Annex-B H.264/HEVC,
//! ADTS AAC) from [`crate::ts`] into [`caudal_core::Frame`]s, building the
//! `avcC` / `hvcC` / `AudioSpecificConfig` init records along the way.
//!
//! Timestamps: TS PTS/DTS are 90 kHz, 33-bit wire values that wrap
//! (~26.5 h) and, for PTS, jitter backwards a little under B-frames.
//! `TsClock` extends the DTS series (always monotonic) to an absolute
//! 90 kHz value per track; PTS is derived from it by a wrapped delta, which
//! stays correct across B-frame reordering without needing its own
//! unwrap state. Every track is then rebased to the first DTS seen on the
//! connection, so playback starts near t=0. Audio's absolute 90 kHz value
//! is converted to its own timescale (the sample rate) at the end.

use bytes::Bytes;
use caudal_core::{AudioParams, Codec, Cue, Frame, TrackId, TrackInfo, VideoParams};
use mp4_atom::{Atom, Avcc, HvcCArray, Hvcc};

use crate::ts::{EsKind, EsUnit};

const VIDEO_TRACK: TrackId = TrackId(0);
const AUDIO_TRACK: TrackId = TrackId(1);
const TS_WRAP: i64 = 1 << 33;
const TS_HALF: i64 = 1 << 32;

pub enum DemuxEvent {
    VideoInit(TrackInfo),
    VideoFrame(Frame),
    AudioInit(TrackInfo),
    AudioFrame(Frame),
    /// An SCTE-35 cue, its splice time mapped onto the same rebased clock
    /// as the frames (so `at_us` matches `TrackInfo::to_micros(dts)`).
    Cue(Cue),
}

/// `a - b` on the 33-bit TS clock, wrapped into `(-2^32, 2^32]`. Used both
/// to detect a clock wrap (extending a monotonic series) and to place a
/// non-monotonic PTS relative to an already-extended DTS.
fn wrapped_diff(a: u64, b: u64) -> i64 {
    let mut diff = (a & (TS_WRAP as u64 - 1)) as i64 - (b & (TS_WRAP as u64 - 1)) as i64;
    if diff < -TS_HALF {
        diff += TS_WRAP;
    } else if diff > TS_HALF {
        diff -= TS_WRAP;
    }
    diff
}

/// Reconstructs an absolute, monotonically extended 90 kHz clock from a
/// series of 33-bit wire timestamps that is itself expected to be
/// monotonic (a DTS series, or audio PTS, which never reorders).
#[derive(Default)]
struct TsClock {
    last_raw: Option<u64>,
    absolute: i64,
}

impl TsClock {
    fn extend(&mut self, raw: u64) -> i64 {
        match self.last_raw {
            None => self.absolute = raw as i64,
            Some(last) => self.absolute += wrapped_diff(raw, last),
        }
        self.last_raw = Some(raw);
        self.absolute
    }
}

struct VideoState {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    vps: Option<Vec<u8>>,
    init: Option<Vec<u8>>,
}

struct AudioState {
    sample_rate: u32,
    asc: Option<Vec<u8>>,
}

pub struct Demuxer {
    zero_point: Option<i64>,
    video_clock: TsClock,
    audio_clock: TsClock,
    video: Option<VideoState>,
    audio: Option<AudioState>,
}

impl Default for Demuxer {
    fn default() -> Self {
        Self::new()
    }
}

impl Demuxer {
    pub fn new() -> Self {
        Self {
            zero_point: None,
            video_clock: TsClock::default(),
            audio_clock: TsClock::default(),
            video: None,
            audio: None,
        }
    }

    pub fn consume(&mut self, unit: EsUnit, out: &mut Vec<DemuxEvent>) {
        match unit.kind {
            EsKind::H264 | EsKind::H265 => self.consume_video(unit, out),
            EsKind::Aac => self.consume_audio(unit, out),
            EsKind::Scte35 => self.consume_cue(unit, out),
        }
    }

    /// Places a `splice_info_section` on the frames' timeline. Its splice
    /// time is a 33-bit wire value like any PTS, so it is unwrapped against
    /// the newest video DTS (audio when there is no video): a cue sent just
    /// before a clock wrap for a splice just after it lands after, not 26.5
    /// hours before. An immediate cue (no splice time) applies at the
    /// newest media time.
    fn consume_cue(&mut self, unit: EsUnit, out: &mut Vec<DemuxEvent>) {
        let splice = match caudal_scte35::parse(&unit.data) {
            Ok(s) => s,
            Err(err) => {
                tracing::debug!(%err, "scte-35 section dropped");
                return;
            }
        };
        let clock = [&self.video_clock, &self.audio_clock].into_iter().find(|c| c.last_raw.is_some());
        let ticks = match (splice.pts_90k, clock) {
            (Some(p), Some(c)) => c.absolute + wrapped_diff(p, c.last_raw.unwrap_or(0)),
            (None, Some(c)) => c.absolute,
            (Some(p), None) => p as i64,
            (None, None) => self.zero_point.unwrap_or(0),
        };
        let zero = *self.zero_point.get_or_insert(ticks);
        out.push(DemuxEvent::Cue(Cue {
            at_us: caudal_scte35::ticks_to_us(ticks - zero),
            section: Bytes::from(unit.data),
            kind: splice.kind,
        }));
    }

    /// Extends the DTS series (falling back to PTS if a unit has no DTS,
    /// e.g. audio) and rebases both to the connection-wide zero point,
    /// returning `(dts, pts)` in 90 kHz.
    fn video_timestamps(&mut self, unit: &EsUnit) -> (i64, i64) {
        let raw_dts = unit.dts.or(unit.pts).unwrap_or(0);
        let raw_pts = unit.pts.unwrap_or(raw_dts);
        let dts_abs = self.video_clock.extend(raw_dts);
        let pts_abs = dts_abs + wrapped_diff(raw_pts, raw_dts);
        let zero = *self.zero_point.get_or_insert(dts_abs);
        (dts_abs - zero, pts_abs - zero)
    }

    fn audio_timestamp_90k(&mut self, unit: &EsUnit) -> i64 {
        let raw = unit.pts.or(unit.dts).unwrap_or(0);
        let abs = self.audio_clock.extend(raw);
        let zero = *self.zero_point.get_or_insert(abs);
        abs - zero
    }

    fn consume_video(&mut self, unit: EsUnit, out: &mut Vec<DemuxEvent>) {
        let h265 = matches!(unit.kind, EsKind::H265);
        let (dts, pts) = self.video_timestamps(&unit);

        let state = self.video.get_or_insert(VideoState { sps: None, pps: None, vps: None, init: None });

        let mut data = Vec::with_capacity(unit.data.len());
        let mut keyframe = false;
        for nal in split_annexb(&unit.data) {
            if nal.is_empty() {
                continue;
            }
            if h265 {
                if nal.len() < 2 {
                    continue;
                }
                let nal_type = (nal[0] >> 1) & 0x3F;
                match nal_type {
                    32 => state.vps = Some(nal.to_vec()),
                    33 => state.sps = Some(nal.to_vec()),
                    34 => state.pps = Some(nal.to_vec()),
                    35 => {} // AUD: stripped
                    16..=21 => {
                        keyframe = true;
                        push_avcc_nal(&mut data, nal);
                    }
                    _ => push_avcc_nal(&mut data, nal),
                }
            } else {
                let nal_type = nal[0] & 0x1F;
                match nal_type {
                    7 => state.sps = Some(nal.to_vec()),
                    8 => state.pps = Some(nal.to_vec()),
                    9 => {} // AUD: stripped
                    5 => {
                        keyframe = true;
                        push_avcc_nal(&mut data, nal);
                    }
                    _ => push_avcc_nal(&mut data, nal),
                }
            }
        }

        let have_params = if h265 {
            state.sps.is_some() && state.pps.is_some() && state.vps.is_some()
        } else {
            state.sps.is_some() && state.pps.is_some()
        };
        if have_params {
            let init = if h265 {
                build_hvcc(state.vps.as_deref().unwrap(), state.sps.as_deref().unwrap(), state.pps.as_deref().unwrap())
            } else {
                build_avcc(state.sps.as_deref().unwrap(), state.pps.as_deref().unwrap())
            };
            if let Some(init) = init {
                if state.init.as_deref() != Some(&init[..]) {
                    let (width, height) = if h265 {
                        h265_dimensions(state.sps.as_deref().unwrap()).unwrap_or((0, 0))
                    } else {
                        h264_dimensions(state.sps.as_deref().unwrap()).unwrap_or((0, 0))
                    };
                    state.init = Some(init.clone());
                    out.push(DemuxEvent::VideoInit(TrackInfo {
                        id: VIDEO_TRACK,
                        codec: if h265 { Codec::H265 } else { Codec::H264 },
                        timescale: 90_000,
                        init: Bytes::from(init),
                        lang: None,
                        video: Some(VideoParams { width, height, fps: None }),
                        audio: None,
                    }));
                }
            }
        }

        if data.is_empty() {
            // Parameter-set-only or AUD-only access unit (or one we failed
            // to reassemble any VCL data for): nothing to play.
            return;
        }
        out.push(DemuxEvent::VideoFrame(Frame { track: VIDEO_TRACK, dts, pts, keyframe, data: Bytes::from(data) }));
    }

    fn consume_audio(&mut self, unit: EsUnit, out: &mut Vec<DemuxEvent>) {
        let base_ts = self.audio_timestamp_90k(&unit);

        // A single PES packet commonly carries more than one ADTS frame
        // (1024 samples each); each subsequent frame's timestamp advances
        // by exactly one frame's worth of samples.
        let mut offset = 0usize;
        let mut frame_index: i64 = 0;
        while offset < unit.data.len() {
            let Some(adts) = AdtsHeader::parse(&unit.data[offset..]) else { break };
            let frame_end = offset + adts.frame_len;
            if frame_end > unit.data.len() || adts.frame_len <= adts.header_len {
                break;
            }
            let sample_rate = adts.sample_rate;
            // A live sample-rate change is not expected from one encoder;
            // the first-seen rate is kept for the life of the connection.
            let state = self.audio.get_or_insert(AudioState { sample_rate, asc: None });
            let asc = adts.audio_specific_config();
            if state.asc.as_deref() != Some(&asc[..]) {
                state.asc = Some(asc.clone());
                out.push(DemuxEvent::AudioInit(TrackInfo {
                    id: AUDIO_TRACK,
                    codec: Codec::Aac,
                    timescale: state.sample_rate,
                    init: Bytes::from(asc),
                    lang: None,
                    video: None,
                    audio: Some(AudioParams { sample_rate: state.sample_rate, channels: adts.channels }),
                }));
            }

            let raw_au = &unit.data[offset + adts.header_len..frame_end];
            // 90 kHz -> the audio track's own timescale (its sample rate),
            // offsetting by whole AAC frames (1024 samples each) for any
            // extra ADTS frame packed into this PES.
            let ts_90k = base_ts + frame_index * (90_000 * 1024 / i64::from(state.sample_rate));
            let ts = (i128::from(ts_90k) * i128::from(state.sample_rate) / 90_000) as i64;
            out.push(DemuxEvent::AudioFrame(Frame {
                track: AUDIO_TRACK,
                dts: ts,
                pts: ts,
                keyframe: true,
                data: Bytes::copy_from_slice(raw_au),
            }));

            offset = frame_end;
            frame_index += 1;
        }
    }
}

/// Appends one NAL as a 4-byte-length-prefixed (AVCC) unit.
fn push_avcc_nal(out: &mut Vec<u8>, nal: &[u8]) {
    out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
    out.extend_from_slice(nal);
}

/// Splits Annex-B bytes into NAL unit slices, without start codes. Tolerates
/// both 3-byte (`00 00 01`) and 4-byte (`00 00 00 01`) start codes; a NAL
/// immediately preceded by a 4-byte start code may keep one extra leading
/// `0x00` from the previous NAL's own trailing padding (see `NOTES.md`) —
/// harmless for every decoder this project targets.
fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut marks = Vec::new();
    let mut i = 0usize;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            marks.push((i, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::with_capacity(marks.len());
    for (idx, &(_, payload_begin)) in marks.iter().enumerate() {
        let end = marks.get(idx + 1).map(|&(code_begin, _)| code_begin).unwrap_or(data.len());
        if payload_begin < end {
            nals.push(&data[payload_begin..end]);
        }
    }
    nals
}

fn build_avcc(sps: &[u8], pps: &[u8]) -> Option<Vec<u8>> {
    let avcc = Avcc::new(sps, pps).ok()?;
    let mut out = Vec::new();
    avcc.encode_body(&mut out).ok()?;
    Some(out)
}

fn h264_dimensions(sps: &[u8]) -> Option<(u32, u32)> {
    let parsed = scuffle_h264::Sps::parse_with_emulation_prevention(std::io::Cursor::new(sps)).ok()?;
    Some((parsed.width() as u32, parsed.height() as u32))
}

/// Builds an `hvcC` from raw VPS/SPS/PPS NAL bytes (each including their
/// 2-byte NAL unit header). The `general_*` profile/tier/level fields sit
/// at fixed byte offsets in the SPS RBSP (nothing variable-length precedes
/// them: NAL header (2) + sps_video_parameter_set_id/max_sub_layers/nesting
/// (1) + profile_tier_level's first 12 bytes), so they are read directly
/// rather than through a full exp-golomb SPS parse.
fn build_hvcc(vps: &[u8], sps: &[u8], pps: &[u8]) -> Option<Vec<u8>> {
    if sps.len() < 15 {
        return None;
    }
    let mut hvcc = Hvcc::new();
    hvcc.general_profile_space = (sps[3] & 0b1100_0000) >> 6;
    hvcc.general_tier_flag = (sps[3] & 0b0010_0000) != 0;
    hvcc.general_profile_idc = sps[3] & 0b0001_1111;
    hvcc.general_profile_compatibility_flags.copy_from_slice(&sps[4..8]);
    hvcc.general_constraint_indicator_flags.copy_from_slice(&sps[8..14]);
    hvcc.general_level_idc = sps[14];
    hvcc.length_size_minus_one = 3;
    hvcc.num_temporal_layers = 1;
    hvcc.temporal_id_nested = true;
    hvcc.arrays = vec![
        HvcCArray { completeness: true, nal_unit_type: 32, nalus: vec![vps.to_vec()] },
        HvcCArray { completeness: true, nal_unit_type: 33, nalus: vec![sps.to_vec()] },
        HvcCArray { completeness: true, nal_unit_type: 34, nalus: vec![pps.to_vec()] },
    ];
    let mut out = Vec::new();
    hvcc.encode_body(&mut out).ok()?;
    Some(out)
}

fn h265_dimensions(sps: &[u8]) -> Option<(u32, u32)> {
    let parsed = scuffle_h265::SpsNALUnit::parse(std::io::Cursor::new(sps)).ok()?;
    Some((parsed.rbsp.cropped_width() as u32, parsed.rbsp.cropped_height() as u32))
}

/// A single parsed ADTS header (ISO/IEC 13818-7 Annex A), enough to build an
/// `AudioSpecificConfig` and to locate one AAC access unit inside the PES
/// payload.
struct AdtsHeader {
    header_len: usize,
    frame_len: usize,
    profile: u8, // audioObjectType - 1
    sampling_frequency_index: u8,
    channel_configuration: u8,
    sample_rate: u32,
    channels: u8,
}

const ADTS_SAMPLE_RATES: [u32; 13] =
    [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

impl AdtsHeader {
    fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 7 || data[0] != 0xFF || (data[1] & 0xF0) != 0xF0 {
            return None;
        }
        let protection_absent = (data[1] & 0x01) != 0;
        let header_len = if protection_absent { 7 } else { 9 };
        let profile = (data[2] & 0xC0) >> 6;
        let sampling_frequency_index = (data[2] & 0x3C) >> 2;
        let channel_configuration = ((data[2] & 0x01) << 2) | ((data[3] & 0xC0) >> 6);
        let frame_len = (((data[3] & 0x03) as usize) << 11) | ((data[4] as usize) << 3) | ((data[5] as usize) >> 5);
        let sample_rate = *ADTS_SAMPLE_RATES.get(sampling_frequency_index as usize)?;
        let channels = match channel_configuration {
            1..=6 => channel_configuration,
            7 => 8,
            _ => return None,
        };
        Some(Self {
            header_len,
            frame_len,
            profile,
            sampling_frequency_index,
            channel_configuration,
            sample_rate,
            channels,
        })
    }

    /// A 2-byte `AudioSpecificConfig` (audioObjectType + samplingFrequencyIndex
    /// + channelConfiguration + a zeroed `GASpecificConfig` tail).
    ///
    /// ISO/IEC 14496-3 1.6.2.1.
    fn audio_specific_config(&self) -> Vec<u8> {
        let audio_object_type = u16::from(self.profile) + 1;
        let n: u16 = ((audio_object_type & 0x1F) << 11)
            | (u16::from(self.sampling_frequency_index & 0x0F) << 7)
            | (u16::from(self.channel_configuration & 0x0F) << 3);
        n.to_be_bytes().to_vec()
    }
}
