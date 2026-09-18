//! FLV tag parsing: turns raw RTMP video/audio/AMF0 payloads into
//! [`caudal_core`] tracks and frames.
//!
//! Only the pieces `caudal-core`'s contract requires are parsed. Anything we
//! don't support (unknown codecs, multitrack, VP8/VP9, ...) is dropped
//! quietly: a malformed or unsupported tag ends that tag, never the
//! connection.

use std::io::Cursor;

use byteorder::{BigEndian, ReadBytesExt};
use bytes::Bytes;
use caudal_core::{AudioParams, Codec, Cue, CueKind, Frame, TrackId, TrackInfo, VideoParams};
use scuffle_amf0::{Amf0Decoder, Amf0Value};
use scuffle_flv::audio::AudioData;
use scuffle_flv::audio::body::AudioTagBody;
use scuffle_flv::audio::body::enhanced::{AudioPacket, ExAudioTagBody};
use scuffle_flv::audio::body::legacy::LegacyAudioTagBody;
use scuffle_flv::audio::body::legacy::aac::AacAudioData;
use scuffle_flv::audio::header::enhanced::AudioFourCc;
use scuffle_flv::video::header::enhanced::{ExVideoTagHeaderContent, VideoFourCc, VideoPacketType};
use scuffle_flv::video::header::legacy::{LegacyVideoTagHeader, LegacyVideoTagHeaderAvcPacket};
use scuffle_flv::video::header::{VideoFrameType, VideoTagHeader, VideoTagHeaderData};

pub(crate) enum VideoEvent {
    Init(TrackInfo),
    Frame(Frame),
}

pub(crate) enum AudioEvent {
    Init(TrackInfo),
    /// Raw AAC access unit. The caller knows the track's sample rate and
    /// computes the timescale-converted timestamp.
    Frame(Bytes),
}

/// Bytes left unread in `cursor`, without consuming them.
fn remaining(cursor: &Cursor<Bytes>) -> Bytes {
    let pos = cursor.position() as usize;
    let buf = cursor.get_ref();
    if pos >= buf.len() { Bytes::new() } else { buf.slice(pos..) }
}

/// FLV's legacy composition time is transmitted as an unsigned 24-bit field,
/// but real values can be negative (two's-complement) once B-frames are in
/// play.
fn u24_to_i32(v: u32) -> i32 {
    let v = v & 0x00FF_FFFF;
    if v & 0x0080_0000 != 0 { (v as i32) - 0x0100_0000 } else { v as i32 }
}

fn video_frame(timestamp_ms: u32, cts: i32, keyframe: bool, data: Bytes) -> Frame {
    let dts = i64::from(timestamp_ms) * 90;
    let pts = (i64::from(timestamp_ms) + i64::from(cts)) * 90;
    Frame { track: TrackId(0), dts, pts, keyframe, data }
}

/// Parses a `VIDEODATA` RTMP message.
pub(crate) fn demux_video(timestamp_ms: u32, data: Bytes) -> Option<VideoEvent> {
    let mut cursor = Cursor::new(data);
    let header = VideoTagHeader::demux(&mut cursor).ok()?;
    let keyframe = header.frame_type == VideoFrameType::KeyFrame;

    match header.data {
        VideoTagHeaderData::Legacy(legacy) => match legacy {
            LegacyVideoTagHeader::AvcPacket(packet) => {
                let raw = remaining(&cursor);
                match packet {
                    LegacyVideoTagHeaderAvcPacket::SequenceHeader => {
                        Some(VideoEvent::Init(video_init(Codec::H264, raw)))
                    }
                    LegacyVideoTagHeaderAvcPacket::Nalu { composition_time_offset } => {
                        let cts = u24_to_i32(composition_time_offset);
                        Some(VideoEvent::Frame(video_frame(timestamp_ms, cts, keyframe, raw)))
                    }
                    LegacyVideoTagHeaderAvcPacket::EndOfSequence | LegacyVideoTagHeaderAvcPacket::Unknown { .. } => {
                        None
                    }
                }
            }
            LegacyVideoTagHeader::VideoCommand(_) | LegacyVideoTagHeader::Other { .. } => None,
        },
        VideoTagHeaderData::Enhanced(ex) => {
            let ExVideoTagHeaderContent::NoMultiTrack(fourcc) = ex.content else {
                // Multitrack video is not supported; drop.
                return None;
            };
            let codec = match fourcc {
                VideoFourCc::Avc => Codec::H264,
                VideoFourCc::Hevc => Codec::H265,
                VideoFourCc::Av1 => Codec::Av1,
                _ => return None,
            };

            match ex.video_packet_type {
                VideoPacketType::SequenceStart => {
                    let raw = remaining(&cursor);
                    Some(VideoEvent::Init(video_init(codec, raw)))
                }
                VideoPacketType::CodedFrames => {
                    let cts = if matches!(codec, Codec::H264 | Codec::H265) {
                        cursor.read_i24::<BigEndian>().ok()?
                    } else {
                        0
                    };
                    let raw = remaining(&cursor);
                    Some(VideoEvent::Frame(video_frame(timestamp_ms, cts, keyframe, raw)))
                }
                VideoPacketType::CodedFramesX => {
                    let raw = remaining(&cursor);
                    Some(VideoEvent::Frame(video_frame(timestamp_ms, 0, keyframe, raw)))
                }
                // SequenceEnd, Metadata, Mpeg2TsSequenceStart, Multitrack, ModEx: nothing to
                // publish.
                _ => None,
            }
        }
    }
}

fn video_init(codec: Codec, raw: Bytes) -> TrackInfo {
    let (width, height) = video_dimensions(codec, &raw).unwrap_or((0, 0));
    TrackInfo {
        id: TrackId(0),
        codec,
        timescale: 90_000,
        init: raw,
        lang: None,
        video: Some(VideoParams { width, height, fps: None }),
        audio: None,
    }
}

fn video_dimensions(codec: Codec, raw: &Bytes) -> Option<(u32, u32)> {
    match codec {
        Codec::H264 => {
            let record = scuffle_h264::AVCDecoderConfigurationRecord::parse(&mut Cursor::new(raw.clone())).ok()?;
            let sps_bytes = record.sps.first()?;
            let sps = scuffle_h264::Sps::parse_with_emulation_prevention(Cursor::new(sps_bytes.clone())).ok()?;
            Some((sps.width() as u32, sps.height() as u32))
        }
        Codec::H265 => {
            let record = scuffle_h265::HEVCDecoderConfigurationRecord::demux(Cursor::new(raw.clone())).ok()?;
            let sps_nalu = record
                .arrays
                .iter()
                .find(|a| a.nal_unit_type == scuffle_h265::NALUnitType::SpsNut)
                .and_then(|a| a.nalus.first())?;
            let sps = scuffle_h265::SpsNALUnit::parse(Cursor::new(sps_nalu.clone())).ok()?;
            Some((sps.rbsp.cropped_width() as u32, sps.rbsp.cropped_height() as u32))
        }
        Codec::Av1 => {
            let record = scuffle_av1::AV1CodecConfigurationRecord::demux(&mut Cursor::new(raw.clone())).ok()?;
            let mut obu_reader = Cursor::new(record.config_obu);
            let obu_header = scuffle_av1::ObuHeader::parse(&mut obu_reader).ok()?;
            let seq = scuffle_av1::seq::SequenceHeaderObu::parse(obu_header, &mut obu_reader).ok()?;
            Some((seq.max_frame_width as u32, seq.max_frame_height as u32))
        }
        _ => None,
    }
}

/// Parses an `AUDIODATA` RTMP message.
pub(crate) fn demux_audio(data: Bytes) -> Option<AudioEvent> {
    let mut cursor = Cursor::new(data);
    let ad = AudioData::demux(&mut cursor).ok()?;

    match ad.body {
        AudioTagBody::Legacy(LegacyAudioTagBody::Aac(aac)) => match aac {
            AacAudioData::SequenceHeader(cfg) => audio_init(cfg).map(AudioEvent::Init),
            AacAudioData::Raw(raw) => Some(AudioEvent::Frame(raw)),
            AacAudioData::Unknown { .. } => None,
        },
        AudioTagBody::Legacy(LegacyAudioTagBody::Other { .. }) => None,
        AudioTagBody::Enhanced(ExAudioTagBody::NoMultitrack { audio_four_cc: AudioFourCc::Aac, packet }) => {
            match packet {
                AudioPacket::SequenceStart { header_data } => audio_init(header_data).map(AudioEvent::Init),
                AudioPacket::CodedFrames { data } => Some(AudioEvent::Frame(data)),
                _ => None,
            }
        }
        _ => None,
    }
}

fn audio_init(asc: Bytes) -> Option<TrackInfo> {
    let cfg = scuffle_aac::PartialAudioSpecificConfig::parse(&asc).ok()?;
    Some(TrackInfo {
        id: TrackId(1),
        codec: Codec::Aac,
        timescale: cfg.sampling_frequency,
        init: asc,
        lang: None,
        video: None,
        audio: Some(AudioParams { sample_rate: cfg.sampling_frequency, channels: cfg.channel_configuration }),
    })
}

/// An SCTE-35 cue from an AMF0 data message: `onCuePoint` (or the
/// `onAdCue` spelling some encoders use), optionally behind
/// `@setDataFrame`. There is no single standard layout (Enhanced RTMP
/// defines none), so this accepts, in order:
///
/// 1. any string field, top level or under `parameters`, holding a whole
///    `splice_info_section` in hex or base64 (it must parse, CRC included);
/// 2. otherwise a `type` / `name` of `cue-out`/`out`/`adStart`/... or
///    `cue-in`/`in`/`adEnd`/..., with an optional `duration` in seconds, for
///    which a `time_signal` section is built.
///
/// The cue is placed at `time` (seconds on the stream clock) when present,
/// else at the message timestamp. RTMP timestamps are milliseconds, and
/// video/audio frames use the same clock, so `at_us` lines up with them.
pub(crate) fn parse_cue_point(timestamp_ms: u32, data: Bytes, event_id: u32) -> Option<Cue> {
    let mut decoder = Amf0Decoder::from_buf(data);
    let values = decoder.decode_all().ok()?;
    let mut values =
        values.into_iter().skip_while(|v| matches!(v, Amf0Value::String(s) if s.as_str() == "@setDataFrame"));
    match values.next()? {
        Amf0Value::String(name)
            if name.as_str().eq_ignore_ascii_case("onCuePoint") || name.as_str().eq_ignore_ascii_case("onAdCue") => {}
        _ => return None,
    }
    let Amf0Value::Object(obj) = values.next()? else { return None };

    // Flatten the top level and `parameters` into one list of fields.
    let mut fields: Vec<(String, Amf0Value<'_>)> = Vec::new();
    for (k, v) in obj.iter() {
        if let (true, Amf0Value::Object(params)) = (k.as_str().eq_ignore_ascii_case("parameters"), v) {
            fields.extend(params.iter().map(|(k, v)| (k.as_str().to_ascii_lowercase(), v.clone())));
        } else {
            fields.push((k.as_str().to_ascii_lowercase(), v.clone()));
        }
    }
    let number = |key: &str| {
        fields.iter().find_map(|(k, v)| match v {
            Amf0Value::Number(n) if k == key && n.is_finite() && *n >= 0.0 => Some(*n),
            Amf0Value::String(s) if k == key => {
                s.as_str().trim().parse::<f64>().ok().filter(|n| n.is_finite() && *n >= 0.0)
            }
            _ => None,
        })
    };
    let at_us = number("time").map_or(i64::from(timestamp_ms) * 1000, |s| (s * 1e6).round() as i64);

    for (_, v) in &fields {
        if let Amf0Value::String(s) = v
            && let Some((section, splice)) = caudal_scte35::decode_text(s.as_str())
        {
            return Some(Cue { at_us, section, kind: splice.kind });
        }
    }

    let word = |key: &str| {
        fields.iter().find_map(|(k, v)| match v {
            Amf0Value::String(s) if k == key => {
                Some(s.as_str().chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_ascii_lowercase())
            }
            _ => None,
        })
    };
    let kind = ["type", "name"].iter().filter_map(|k| word(k)).find_map(|w| match w.as_str() {
        "cueout" | "out" | "adstart" | "breakstart" | "spliceout" | "start" => {
            let duration_us = number("duration").map(|s| (s * 1e6).round() as i64);
            Some(CueKind::Out { duration_us })
        }
        "cuein" | "in" | "adend" | "breakend" | "splicein" | "end" | "return" => Some(CueKind::In),
        _ => None,
    })?;
    let pts = (caudal_scte35::us_to_ticks(at_us) as u64) & caudal_scte35::PTS_MASK;
    let section = caudal_scte35::build(kind, Some(pts), event_id, caudal_scte35::Command::TimeSignal).ok()?;
    Some(Cue { at_us, section, kind })
}

/// Best-effort `framerate` extraction from an `onMetaData` AMF0 payload.
pub(crate) fn parse_metadata_fps(data: Bytes) -> Option<f64> {
    let mut decoder = Amf0Decoder::from_buf(data);
    let values = decoder.decode_all().ok()?;
    for value in values {
        if let Amf0Value::Object(obj) = value {
            for (key, val) in obj.iter() {
                let key = key.as_str();
                if key.eq_ignore_ascii_case("framerate") || key.eq_ignore_ascii_case("fps") {
                    if let Amf0Value::Number(n) = val {
                        return Some(*n);
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use scuffle_amf0::{Amf0Encoder, Amf0Object};

    use super::*;

    fn message(set_data_frame: bool, name: &str, obj: Amf0Object<'_>) -> Bytes {
        let mut buf = Vec::new();
        let mut enc = Amf0Encoder::new(&mut buf);
        if set_data_frame {
            enc.encode_string("@setDataFrame").unwrap();
        }
        enc.encode_string(name).unwrap();
        enc.encode_object(&obj).unwrap();
        Bytes::from(buf)
    }

    fn obj<'a>(fields: Vec<(&'a str, Amf0Value<'a>)>) -> Amf0Object<'a> {
        fields.into_iter().map(|(k, v)| (k.into(), v)).collect()
    }

    #[test]
    fn a_named_cue_out_gets_a_built_section() {
        let data = message(
            false,
            "onCuePoint",
            obj(vec![
                ("name", Amf0Value::String("CUE-OUT".into())),
                ("time", Amf0Value::Number(12.5)),
                ("duration", Amf0Value::Number(30.0)),
            ]),
        );
        let cue = parse_cue_point(99_000, data, 1).unwrap();
        assert_eq!(cue.at_us, 12_500_000, "time wins over the message timestamp");
        assert_eq!(cue.kind, CueKind::Out { duration_us: Some(30_000_000) });
        let s = caudal_scte35::parse(&cue.section).unwrap();
        assert_eq!(s.kind, cue.kind);
        assert_eq!(s.pts_90k, Some(12_500_000 * 9 / 100));
    }

    #[test]
    fn a_section_in_parameters_is_passed_through() {
        let kind = CueKind::Out { duration_us: Some(60_000_000) };
        let section = caudal_scte35::build(kind, Some(1234), 5, caudal_scte35::Command::SpliceInsert).unwrap();
        let hex = caudal_scte35::to_hex(&section);
        let params = obj(vec![("cue", Amf0Value::String(hex.as_str().into()))]);
        let data = message(
            true,
            "onCuePoint",
            obj(vec![("name", Amf0Value::String("scte35".into())), ("parameters", Amf0Value::Object(params))]),
        );
        let cue = parse_cue_point(4_000, data, 1).unwrap();
        assert_eq!(cue.section, section, "bytes unchanged");
        assert_eq!(cue.kind, kind);
        assert_eq!(cue.at_us, 4_000_000, "no time field: the message timestamp");
    }

    #[test]
    fn cue_in_and_non_cues() {
        let data = message(false, "onAdCue", obj(vec![("type", Amf0Value::String("cue-in".into()))]));
        assert_eq!(parse_cue_point(1, data, 1).unwrap().kind, CueKind::In);
        let meta = message(true, "onMetaData", obj(vec![("framerate", Amf0Value::Number(30.0))]));
        assert!(parse_cue_point(1, meta.clone(), 1).is_none());
        assert_eq!(parse_metadata_fps(meta), Some(30.0));
        let unknown = message(false, "onCuePoint", obj(vec![("name", Amf0Value::String("chapter".into()))]));
        assert!(parse_cue_point(1, unknown, 1).is_none());
        assert!(parse_cue_point(1, Bytes::from_static(&[0xFF, 0x00]), 1).is_none());
    }
}
