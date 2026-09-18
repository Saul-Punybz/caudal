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
use caudal_core::{Codec, Frame, TrackId, TrackInfo, VideoParams, AudioParams};
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
                    LegacyVideoTagHeaderAvcPacket::EndOfSequence
                    | LegacyVideoTagHeaderAvcPacket::Unknown { .. } => None,
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
        AudioTagBody::Enhanced(ExAudioTagBody::NoMultitrack {
            audio_four_cc: AudioFourCc::Aac,
            packet,
        }) => match packet {
            AudioPacket::SequenceStart { header_data } => audio_init(header_data).map(AudioEvent::Init),
            AudioPacket::CodedFrames { data } => Some(AudioEvent::Frame(data)),
            _ => None,
        },
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
