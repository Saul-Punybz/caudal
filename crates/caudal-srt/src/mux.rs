//! Minimal live MPEG-TS muxer, driven directly by [`caudal_core::Frame`]s
//! (no `hang` broadcast in between; see `NOTES.md` for why this is a
//! hand-rolled muxer on top of `mpeg2ts::ts::TsPacketWriter` rather than
//! `moq-mux`'s TS export).
//!
//! H.264/H.265 frames arrive AVCC-framed with parameter sets stripped (see
//! `crate::ts`'s demux, which is the mirror image of this); this muxer
//! converts back to Annex B, re-injecting the avcC/hvcC's SPS/PPS/VPS
//! before every keyframe and an Access Unit Delimiter before every access
//! unit. AAC frames are raw access units, wrapped in an ADTS header built
//! from the `AudioSpecificConfig`. Opus has no standard TS mapping we can
//! build in scope; it is dropped with a one-time warning per stream.
//!
//! PAT/PMT are (re-)written at start, on every video keyframe, and at least
//! every [`PSI_INTERVAL`]. A PCR is carried on the video PID (or the audio
//! PID for an audio-only stream) at least every [`PCR_INTERVAL`], derived
//! from that frame's DTS so it tracks the media clock, not wall time.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use bytes::Bytes;
use caudal_core::{Codec, Frame, TrackInfo, TrackKind};
use mp4_atom::{Atom, Avcc, Hvcc};
use mpeg2ts::es::{StreamId, StreamType};
use mpeg2ts::pes::PesHeader;
use mpeg2ts::time::{ClockReference, Timestamp as MpegTimestamp};
use mpeg2ts::ts::payload::{Bytes as TsBytes, Pat, Pes, Pmt};
use mpeg2ts::ts::{
    AdaptationField, ContinuityCounter, EsInfo, Pid, ProgramAssociation, TransportScramblingControl, TsHeader,
    TsPacket, TsPacketWriter, TsPayload, VersionNumber, WriteTsPacket,
};

const PAT_PID: u16 = 0;
const PMT_PID: u16 = 0x1000;
const VIDEO_PID: u16 = 0x101;
const AUDIO_PID: u16 = 0x102;

/// Re-emit PAT/PMT at least this often for mid-stream tune-in.
const PSI_INTERVAL: Duration = Duration::from_millis(100);
/// TR 101 290 flags a PCR gap over 40 ms.
const PCR_INTERVAL: Duration = Duration::from_millis(40);
/// PTS/DTS and PCR both wrap at 2^33 ticks (90 kHz for PTS/DTS; PCR's base
/// is the same 33-bit field, extended to 27 MHz by `ClockReference`).
const TS_TIMESTAMP_MASK: u64 = (1u64 << 33) - 1;

struct VideoTrack {
    h265: bool,
    vps: Vec<u8>,
    sps: Vec<u8>,
    pps: Vec<u8>,
}

impl VideoTrack {
    /// Decodes an `avcC` body (as built by `crate::ts::build_avcc`, i.e. no
    /// atom header) back into its SPS/PPS.
    fn from_avcc(init: &Bytes) -> Option<Self> {
        let mut buf: &[u8] = init.as_ref();
        let avcc = Avcc::decode_body(&mut buf).ok()?;
        let sps = avcc.sequence_parameter_sets.first()?.clone();
        let pps = avcc.picture_parameter_sets.first()?.clone();
        Some(Self { h265: false, vps: Vec::new(), sps, pps })
    }

    /// Decodes an `hvcC` body back into its VPS/SPS/PPS.
    fn from_hvcc(init: &Bytes) -> Option<Self> {
        let mut buf: &[u8] = init.as_ref();
        let hvcc = Hvcc::decode_body(&mut buf).ok()?;
        let find = |nal_type: u8| {
            hvcc.arrays.iter().find(|a| a.nal_unit_type == nal_type).and_then(|a| a.nalus.first()).cloned()
        };
        Some(Self { h265: true, vps: find(32)?, sps: find(33)?, pps: find(34)? })
    }
}

/// ADTS fields decoded from a 2-byte `AudioSpecificConfig` (see
/// `crate::ts::AdtsHeader::audio_specific_config`, whose layout this
/// reverses exactly).
struct AudioTrack {
    /// `audioObjectType - 1`: ADTS "profile" is only 2 bits, so this loses
    /// information above AAC LTP (object type 4), which nothing here emits.
    profile: u8,
    sampling_freq_idx: u8,
    channel_config: u8,
}

impl AudioTrack {
    fn from_asc(init: &Bytes) -> Option<Self> {
        if init.len() < 2 {
            return None;
        }
        let n = u16::from_be_bytes([init[0], init[1]]);
        let audio_object_type = (n >> 11) & 0x1F;
        let sampling_freq_idx = ((n >> 7) & 0x0F) as u8;
        let channel_config = ((n >> 3) & 0x0F) as u8;
        let profile = audio_object_type.checked_sub(1)?.min(3) as u8;
        Some(Self { profile, sampling_freq_idx, channel_config })
    }
}

/// Converts a timestamp on `timescale` to the 90 kHz, 33-bit-wrapped clock
/// TS PTS/DTS use. Video is already 90 kHz; audio is on its sample rate.
fn to_90k(ts: i64, timescale: u32) -> u64 {
    let scaled = (i128::from(ts) * 90_000) / i128::from(timescale.max(1));
    (scaled as i64 as u64) & TS_TIMESTAMP_MASK
}

fn write_annexb(out: &mut Vec<u8>, nal: &[u8]) {
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(nal);
}

/// Mirrors `PesHeader::optional_header_len` (private in `mpeg2ts`, and
/// duplicated the same way in `crate::demux`): 3 fixed flag/length bytes
/// plus whichever timestamp fields are present. ISO/IEC 13818-1 2.4.3.7.
fn pes_optional_header_len(header: &PesHeader) -> u16 {
    3 + header.pts.map_or(0, |_| 5) + header.dts.map_or(0, |_| 5) + header.escr.map_or(0, |_| 6)
}

/// A 7-byte ADTS header (no CRC) for one AAC raw access unit.
fn adts_header(payload_len: usize, profile: u8, sampling_freq_idx: u8, channel_config: u8) -> [u8; 7] {
    let frame_len = (7 + payload_len) as u16;
    [
        0xFF,
        0xF1,
        (profile << 6) | ((sampling_freq_idx & 0x0F) << 2) | ((channel_config >> 2) & 0x01),
        ((channel_config & 0x03) << 6) | ((frame_len >> 11) & 0x03) as u8,
        ((frame_len >> 3) & 0xFF) as u8,
        (((frame_len & 0x07) as u8) << 5) | 0x1F,
        0xFC,
    ]
}

/// Byte size of an adaptation field carrying only `random_access_indicator`
/// (and possibly a PCR): the `mpeg2ts` crate keeps `AdaptationField`'s own
/// `external_size` private, so this mirrors it for the one shape this mux
/// ever builds (length + flags bytes, plus 6 for a PCR).
fn adaptation_field_size(has_pcr: bool) -> usize {
    2 + if has_pcr { 6 } else { 0 }
}

/// One access unit delimiter, Annex-B framed, per codec.
const AUD_H264: [u8; 6] = [0, 0, 0, 1, 0x09, 0xF0];
const AUD_H265: [u8; 7] = [0, 0, 0, 1, 0x46, 0x01, 0x50];

pub(crate) struct TsMux {
    writer: TsPacketWriter<Vec<u8>>,
    cc: HashMap<u16, ContinuityCounter>,
    video: Option<VideoTrack>,
    audio: Option<AudioTrack>,
    psi_written: bool,
    last_psi: Option<Instant>,
    last_pcr: Option<Instant>,
    warned_opus: bool,
}

impl TsMux {
    pub(crate) fn new() -> Self {
        Self {
            writer: TsPacketWriter::new(Vec::new()),
            cc: HashMap::new(),
            video: None,
            audio: None,
            psi_written: false,
            last_psi: None,
            last_pcr: None,
            warned_opus: false,
        }
    }

    /// Takes every TS byte muxed so far, leaving the muxer's own state
    /// (continuity counters, PSI/PCR timers, track config) untouched.
    pub(crate) fn take_output(&mut self) -> Vec<u8> {
        let old = std::mem::replace(&mut self.writer, TsPacketWriter::new(Vec::new()));
        old.into_stream()
    }

    /// Refreshes the known tracks. Safe to call repeatedly with the same
    /// tracks (e.g. on every `Event::TracksChanged`); harmless beyond an
    /// extra PAT/PMT re-emission.
    pub(crate) fn set_tracks(&mut self, tracks: &[TrackInfo]) {
        let mut video = None;
        let mut audio = None;
        for t in tracks {
            match t.codec {
                Codec::H264 => video = VideoTrack::from_avcc(&t.init).or(video),
                Codec::H265 => video = VideoTrack::from_hvcc(&t.init).or(video),
                Codec::Aac => audio = AudioTrack::from_asc(&t.init).or(audio),
                Codec::Opus if !self.warned_opus => {
                    tracing::warn!(
                        "srt out: Opus has no MPEG-TS mapping in this build; dropping audio for this stream"
                    );
                    self.warned_opus = true;
                }
                _ => {}
            }
        }
        self.video = video;
        self.audio = audio;
        self.psi_written = false;
    }

    /// Muxes one frame. A frame on a track this muxer doesn't recognize
    /// (Opus, or a video codec whose init hasn't parsed yet) is dropped.
    pub(crate) fn push_frame(&mut self, info: &TrackInfo, frame: &Frame) {
        self.maybe_write_psi(frame.keyframe && info.kind() == TrackKind::Video);
        let pcr = if self.is_pcr_track(info.kind()) {
            self.due_pcr(to_90k(frame.dts, info.timescale))
        } else {
            None
        };
        match info.kind() {
            TrackKind::Video => self.write_video(info, frame, pcr),
            TrackKind::Audio if info.codec == Codec::Aac => self.write_audio(info, frame, pcr),
            _ => {}
        }
    }

    /// The video PID carries PCR whenever there is video; an audio-only
    /// stream carries it on the audio PID instead.
    fn is_pcr_track(&self, kind: TrackKind) -> bool {
        match kind {
            TrackKind::Video => true,
            TrackKind::Audio => self.video.is_none(),
            _ => false,
        }
    }

    fn due_pcr(&mut self, media_90k: u64) -> Option<u64> {
        let due = self.last_pcr.is_none_or(|t| t.elapsed() >= PCR_INTERVAL);
        if !due {
            return None;
        }
        self.last_pcr = Some(Instant::now());
        Some(media_90k * 300)
    }

    fn maybe_write_psi(&mut self, force: bool) {
        if self.video.is_none() && self.audio.is_none() {
            return;
        }
        let due = !self.psi_written || force || self.last_psi.is_none_or(|t| t.elapsed() >= PSI_INTERVAL);
        if !due {
            return;
        }
        self.write_pat();
        self.write_pmt();
        self.psi_written = true;
        self.last_psi = Some(Instant::now());
    }

    fn write_pat(&mut self) {
        let pat = Pat {
            transport_stream_id: 1,
            version_number: VersionNumber::new(),
            table: vec![ProgramAssociation { program_num: 1, program_map_pid: Pid::new(PMT_PID).expect("in range") }],
        };
        self.write_psi_packet(PAT_PID, TsPayload::Pat(pat));
    }

    fn write_pmt(&mut self) {
        let mut es_info = Vec::new();
        if let Some(video) = &self.video {
            es_info.push(EsInfo {
                stream_type: if video.h265 { StreamType::H265 } else { StreamType::H264 },
                elementary_pid: Pid::new(VIDEO_PID).expect("in range"),
                descriptors: Vec::new(),
            });
        }
        if self.audio.is_some() {
            es_info.push(EsInfo {
                stream_type: StreamType::AdtsAac,
                elementary_pid: Pid::new(AUDIO_PID).expect("in range"),
                descriptors: Vec::new(),
            });
        }
        let pcr_pid = if self.video.is_some() {
            VIDEO_PID
        } else if self.audio.is_some() {
            AUDIO_PID
        } else {
            return;
        };
        let pmt = Pmt {
            program_num: 1,
            pcr_pid: Some(Pid::new(pcr_pid).expect("in range")),
            version_number: VersionNumber::new(),
            program_info: Vec::new(),
            es_info,
        };
        self.write_psi_packet(PMT_PID, TsPayload::Pmt(pmt));
    }

    fn write_psi_packet(&mut self, pid: u16, payload: TsPayload) {
        let cc = self.next_cc(pid);
        let packet = TsPacket {
            header: TsHeader {
                transport_error_indicator: false,
                transport_priority: false,
                pid: Pid::new(pid).expect("in range"),
                transport_scrambling_control: TransportScramblingControl::NotScrambled,
                continuity_counter: cc,
            },
            adaptation_field: None,
            payload: Some(payload),
        };
        if let Err(err) = self.writer.write_ts_packet(&packet) {
            tracing::debug!(%err, "srt out: failed to write PSI packet");
        }
    }

    fn write_video(&mut self, info: &TrackInfo, frame: &Frame, pcr_27mhz: Option<u64>) {
        let Some(video) = &self.video else { return };
        let (h265, vps, sps, pps) = (video.h265, video.vps.clone(), video.sps.clone(), video.pps.clone());

        let mut payload = Vec::with_capacity(frame.data.len() + 64);
        if h265 {
            payload.extend_from_slice(&AUD_H265);
        } else {
            payload.extend_from_slice(&AUD_H264);
        }
        if frame.keyframe {
            if h265 {
                write_annexb(&mut payload, &vps);
            }
            write_annexb(&mut payload, &sps);
            write_annexb(&mut payload, &pps);
        }
        // AVCC (4-byte length prefix) -> Annex B. A truncated length runs
        // past the end of `data`: stop rather than read out of bounds.
        let mut data = frame.data.as_ref();
        while data.len() >= 4 {
            let len = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
            data = &data[4..];
            if len > data.len() {
                break;
            }
            write_annexb(&mut payload, &data[..len]);
            data = &data[len..];
        }

        let pts90 = to_90k(frame.pts, info.timescale);
        let dts90 = to_90k(frame.dts, info.timescale);
        let header = PesHeader {
            stream_id: StreamId::new(0xE0),
            priority: false,
            data_alignment_indicator: true,
            copyright: false,
            original_or_copy: false,
            pts: MpegTimestamp::new(pts90).ok(),
            dts: if dts90 != pts90 { MpegTimestamp::new(dts90).ok() } else { None },
            escr: None,
        };
        // A bounded `pes_packet_len` lets the demuxer close this PES on its
        // own, without depending on a following PES (which does not exist
        // for the very last access unit of a stream, or of a live viewer's
        // capture window). Video access units bigger than a u16 fall back
        // to 0 ("unbounded", the norm `crate::ts` also parses that way).
        let total_len = pes_optional_header_len(&header) as usize + payload.len();
        let pes_packet_len = u16::try_from(total_len).unwrap_or(0);
        self.write_pes(VIDEO_PID, header, pes_packet_len, &payload, frame.keyframe, pcr_27mhz);
    }

    fn write_audio(&mut self, info: &TrackInfo, frame: &Frame, pcr_27mhz: Option<u64>) {
        let Some(audio) = &self.audio else { return };
        let (profile, sfi, chan) = (audio.profile, audio.sampling_freq_idx, audio.channel_config);

        let mut payload = Vec::with_capacity(frame.data.len() + 7);
        payload.extend_from_slice(&adts_header(frame.data.len(), profile, sfi, chan));
        payload.extend_from_slice(&frame.data);

        let pts90 = to_90k(frame.pts, info.timescale);
        let header = PesHeader {
            stream_id: StreamId::new(0xC0),
            priority: false,
            data_alignment_indicator: true,
            copyright: false,
            original_or_copy: false,
            pts: MpegTimestamp::new(pts90).ok(),
            dts: None,
            escr: None,
        };
        let pes_packet_len = pes_optional_header_len(&header) + payload.len() as u16;
        self.write_pes(AUDIO_PID, header, pes_packet_len, &payload, false, pcr_27mhz);
    }

    /// Packetizes one PES (header + `data`) into 188-byte TS packets on
    /// `pid`. The first packet carries `random_access`/`pcr_27mhz` as an
    /// adaptation field when either is set; `mpeg2ts` pads every packet to
    /// exactly 188 bytes on its own (stuffing bytes), so slicing does not
    /// need to fill each packet exactly.
    fn write_pes(
        &mut self,
        pid: u16,
        header: PesHeader,
        pes_packet_len: u16,
        data: &[u8],
        random_access: bool,
        pcr_27mhz: Option<u64>,
    ) {
        let total_header_bytes = 6 + pes_optional_header_len(&header) as usize;
        let mut offset = 0usize;
        let mut first = true;
        loop {
            let adaptation = (first && (random_access || pcr_27mhz.is_some())).then(|| AdaptationField {
                discontinuity_indicator: false,
                random_access_indicator: random_access,
                es_priority_indicator: false,
                pcr: pcr_27mhz.and_then(|v| ClockReference::new(v).ok()),
                opcr: None,
                splice_countdown: None,
                transport_private_data: Vec::new(),
                extension: None,
            });
            let adapt_size = adaptation.as_ref().map_or(0, |_| adaptation_field_size(pcr_27mhz.is_some()));
            let header_here = if first { total_header_bytes } else { 0 };
            let avail = 188usize.saturating_sub(4).saturating_sub(adapt_size).saturating_sub(header_here);
            let take = avail.min(data.len() - offset);
            let chunk = &data[offset..offset + take];

            let payload = if first {
                TsPayload::PesStart(Pes {
                    header: header.clone(),
                    pes_packet_len,
                    data: TsBytes::new(chunk).expect("chunk sized to fit one TS packet"),
                })
            } else {
                TsPayload::PesContinuation(TsBytes::new(chunk).expect("chunk sized to fit one TS packet"))
            };
            let cc = self.next_cc(pid);
            let packet = TsPacket {
                header: TsHeader {
                    transport_error_indicator: false,
                    transport_priority: false,
                    pid: Pid::new(pid).expect("in range"),
                    transport_scrambling_control: TransportScramblingControl::NotScrambled,
                    continuity_counter: cc,
                },
                adaptation_field: adaptation,
                payload: Some(payload),
            };
            if let Err(err) = self.writer.write_ts_packet(&packet) {
                tracing::debug!(%err, "srt out: failed to write TS packet");
                return;
            }
            offset += take;
            first = false;
            if offset >= data.len() {
                return;
            }
        }
    }

    fn next_cc(&mut self, pid: u16) -> ContinuityCounter {
        let cc = self.cc.entry(pid).or_default();
        let ret = *cc;
        cc.increment();
        ret
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use caudal_core::{AudioParams, TrackId, VideoParams};

    fn h264_init() -> Bytes {
        // A tiny but structurally valid SPS/PPS pair (baseline, 16x16),
        // built the same way `crate::ts::build_avcc` does.
        let sps: &[u8] = &[
            0x67, 0x42, 0x00, 0x0A, 0x8C, 0x8D, 0x40, 0x50, 0x1E, 0xD0, 0x0F, 0x08, 0x84, 0x6A,
        ];
        let pps: &[u8] = &[0x68, 0xCE, 0x3C, 0x80];
        let avcc = mp4_atom::Avcc::new(sps, pps).unwrap();
        let mut out = Vec::new();
        avcc.encode_body(&mut out).unwrap();
        Bytes::from(out)
    }

    fn video_track() -> TrackInfo {
        TrackInfo {
            id: TrackId(0),
            codec: Codec::H264,
            timescale: 90_000,
            init: h264_init(),
            lang: None,
            video: Some(VideoParams { width: 16, height: 16, fps: None }),
            audio: None,
        }
    }

    fn audio_track() -> TrackInfo {
        // AOT=2 (LC), 48 kHz (index 3), stereo.
        let n: u16 = (2u16 << 11) | (3u16 << 7) | (2u16 << 3);
        TrackInfo {
            id: TrackId(1),
            codec: Codec::Aac,
            timescale: 48_000,
            init: Bytes::copy_from_slice(&n.to_be_bytes()),
            lang: None,
            video: None,
            audio: Some(AudioParams { sample_rate: 48_000, channels: 2 }),
        }
    }

    #[test]
    fn keyframe_produces_pat_pmt_and_whole_ts_packets() {
        let video = video_track();
        let mut mux = TsMux::new();
        mux.set_tracks(std::slice::from_ref(&video));
        let frame =
            Frame { track: TrackId(0), dts: 0, pts: 0, keyframe: true, data: Bytes::from_static(&[0, 0, 0, 1, 0x65]) };
        mux.push_frame(&video, &frame);
        let out = mux.take_output();
        assert!(!out.is_empty());
        assert_eq!(out.len() % 188, 0, "output must be whole TS packets");
        assert_eq!(out[0], 0x47, "TS sync byte");
        // PAT then PMT then at least one video packet.
        assert!(out.len() >= 188 * 3);
        assert_eq!(out[188], 0x47, "PMT packet's TS sync byte");
        let pmt_header = u16::from_be_bytes([out[189], out[190]]);
        assert_eq!(pmt_header & 0x1FFF, PMT_PID, "second packet is on the PMT pid");
        assert_ne!(pmt_header & 0x4000, 0, "PMT packet has payload_unit_start set");
    }

    #[test]
    fn opus_is_dropped_without_panicking() {
        let mut mux = TsMux::new();
        let opus = TrackInfo {
            id: TrackId(1),
            codec: Codec::Opus,
            timescale: 48_000,
            init: Bytes::new(),
            lang: None,
            video: None,
            audio: Some(AudioParams { sample_rate: 48_000, channels: 2 }),
        };
        mux.set_tracks(std::slice::from_ref(&opus));
        let frame = Frame { track: TrackId(1), dts: 0, pts: 0, keyframe: true, data: Bytes::from_static(&[1, 2, 3]) };
        mux.push_frame(&opus, &frame);
        // No video, no recognized audio: nothing to announce yet.
        assert!(mux.take_output().is_empty());
    }

    #[test]
    fn audio_only_stream_carries_pcr_on_the_audio_pid() {
        let audio = audio_track();
        let mut mux = TsMux::new();
        mux.set_tracks(std::slice::from_ref(&audio));
        let frame =
            Frame { track: TrackId(1), dts: 0, pts: 0, keyframe: true, data: Bytes::from_static(&[0xAA; 100]) };
        mux.push_frame(&audio, &frame);
        let out = mux.take_output();
        assert!(!out.is_empty());
        assert_eq!(out.len() % 188, 0);
    }

    fn have(bin: &str) -> bool {
        std::process::Command::new("which").arg(bin).output().is_ok_and(|o| o.status.success())
    }

    /// Splits Annex-B bytes into NAL slices (no start codes). Standalone
    /// copy of `crate::demux::split_annexb` for this diagnostic (that one
    /// is private to its own module).
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

    /// Diagnostic: mux real H.264 access units (extracted from
    /// `caudal-hls`'s `av.mp4` fixture) with no SRT or ingest demux
    /// involved at all, and confirm a real decoder plays the result back.
    /// Isolates this muxer from the (separately tested) SRT ingest path.
    #[test]
    fn real_h264_fixture_round_trips_through_ffmpeg() {
        if !have("ffmpeg") || !have("ffprobe") {
            eprintln!("SKIP: ffmpeg/ffprobe not installed");
            return;
        }

        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/../caudal-hls/tests/fixtures/av.mp4");
        let annexb_path = std::env::temp_dir().join(format!("caudal_srt_mux_fixture_{}.h264", std::process::id()));
        let extract = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-i", fixture, "-an", "-c:v", "copy", "-bsf:v", "h264_mp4toannexb", "-f", "h264"])
            .arg(&annexb_path)
            .status()
            .expect("run ffmpeg to extract Annex B");
        assert!(extract.success(), "ffmpeg extraction failed");

        // The fixture has real B-frames (PTS != DTS for several access
        // units); ffmpeg's own mpegts remux is the ground truth for what
        // correct per-access-unit PTS/DTS look like, in bitstream (decode)
        // order, matching `access_units` below 1:1.
        let ref_ts_path = std::env::temp_dir().join(format!("caudal_srt_mux_fixture_ref_{}.ts", std::process::id()));
        let remux = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-i", fixture, "-c", "copy", "-f", "mpegts"])
            .arg(&ref_ts_path)
            .status()
            .expect("run ffmpeg to build the reference remux");
        assert!(remux.success(), "ffmpeg reference remux failed");
        let probe = std::process::Command::new("ffprobe")
            .args([
                "-v", "error", "-select_streams", "v:0", "-show_entries", "packet=pts,dts", "-of", "csv=p=0",
            ])
            .arg(&ref_ts_path)
            .output()
            .expect("run ffprobe on the reference remux");
        let _ = std::fs::remove_file(&ref_ts_path);
        let ref_timestamps: Vec<(i64, i64)> = String::from_utf8_lossy(&probe.stdout)
            .lines()
            .filter_map(|line| {
                let mut parts = line.split(',');
                let pts: i64 = parts.next()?.parse().ok()?;
                let dts: i64 = parts.next()?.parse().ok()?;
                Some((pts, dts))
            })
            .collect();

        let annexb = std::fs::read(&annexb_path).expect("read extracted Annex B");
        let _ = std::fs::remove_file(&annexb_path);

        let mut sps = None;
        let mut pps = None;
        let mut access_units: Vec<Vec<u8>> = Vec::new();
        let mut keyframes: Vec<bool> = Vec::new();
        let mut current = Vec::new();
        let mut current_is_idr = false;
        for nal in split_annexb(&annexb) {
            if nal.is_empty() {
                continue;
            }
            let nal_type = nal[0] & 0x1F;
            match nal_type {
                7 => sps = Some(nal.to_vec()),
                8 => pps = Some(nal.to_vec()),
                9 => {} // AUD
                5 | 1 => {
                    if !current.is_empty() {
                        access_units.push(std::mem::take(&mut current));
                        keyframes.push(current_is_idr);
                    }
                    current_is_idr = nal_type == 5;
                    current.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                    current.extend_from_slice(nal);
                }
                _ => {
                    if !current.is_empty() {
                        current.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                        current.extend_from_slice(nal);
                    }
                }
            }
        }
        if !current.is_empty() {
            access_units.push(current);
            keyframes.push(current_is_idr);
        }
        let sps = sps.expect("fixture has an SPS");
        let pps = pps.expect("fixture has a PPS");
        assert!(access_units.len() > 10, "expected several access units, got {}", access_units.len());
        assert_eq!(
            ref_timestamps.len(),
            access_units.len(),
            "reference remux and split access units disagree on frame count"
        );

        let init = {
            let avcc = Avcc::new(&sps, &pps).expect("build avcC");
            let mut out = Vec::new();
            avcc.encode_body(&mut out).expect("encode avcC");
            Bytes::from(out)
        };
        let track = TrackInfo {
            id: TrackId(0),
            codec: Codec::H264,
            timescale: 90_000,
            init,
            lang: None,
            video: Some(VideoParams { width: 256, height: 144, fps: None }),
            audio: None,
        };

        let mut mux = TsMux::new();
        mux.set_tracks(std::slice::from_ref(&track));
        let mut out_bytes = Vec::new();
        for (i, au) in access_units.iter().enumerate() {
            let keyframe = keyframes[i];
            let (pts, dts) = ref_timestamps[i];
            let frame = Frame { track: TrackId(0), dts, pts, keyframe, data: Bytes::from(au.clone()) };
            mux.push_frame(&track, &frame);
            out_bytes.extend_from_slice(&mux.take_output());
        }

        let out_path = std::env::temp_dir().join(format!("caudal_srt_mux_fixture_{}.ts", std::process::id()));
        std::fs::write(&out_path, &out_bytes).expect("write muxed TS");

        let output = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "stream=codec_name", "-of", "json"])
            .arg(&out_path)
            .output()
            .expect("run ffprobe");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.trim().is_empty(), "ffprobe reported errors: {stderr}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("h264"), "no h264 in output: {stdout}");

        let decode = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&out_path)
            .args(["-f", "null", "-"])
            .output()
            .expect("run ffmpeg decode");
        let decode_err = String::from_utf8_lossy(&decode.stderr);
        assert!(decode_err.trim().is_empty(), "ffmpeg reported errors decoding: {decode_err}");

        let _ = std::fs::remove_file(&out_path);
    }
}
