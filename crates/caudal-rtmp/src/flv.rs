//! Builds RTMP `VIDEODATA`/`AUDIODATA` message payloads (FLV tag bodies) for
//! outbound frames: what `caudal-restream` pushes and what RTMP play
//! ([`crate::play`]) sends viewers. Also the `onMetaData` a player gets.
//!
//! Byte layouts match what `crates/caudal-rtmp/src/demux.rs` reads on the
//! way in (legacy AVC/AAC tags per the FLV spec, Enhanced RTMP v2 FourCC
//! tags for HEVC/AV1/Opus per <https://github.com/veovera/enhanced-rtmp>),
//! confirmed against `scuffle-flv`'s demux source rather than assumed. The
//! loopback test round-trips through `caudal-rtmp` itself, so a byte
//! mismatch here fails a test, not a real push.
//!
//! Legacy H.264 keeps dts/pts fidelity via the AVC packet's 24-bit
//! composition time offset. Enhanced HEVC frames with B-frame reordering
//! are sent as `CodedFrames` (packet type 1), which carries the same
//! offset; HEVC frames without one, and every AV1 frame (AV1 has no
//! reordering on the wire), as `CodedFramesX` (packet type 3, no offset).

use bytes::{BufMut, Bytes, BytesMut};
use caudal_core::{Codec, Frame, TrackInfo};

const AVC_CODEC_ID: u8 = 7; // scuffle_flv::video::header::legacy::VideoCodecId::Avc
const AVC_SEQ_HDR: u8 = 0;
const AVC_NALU: u8 = 1;

const AAC_SOUND_FORMAT: u8 = 10; // scuffle_flv::audio::header::legacy::SoundFormat::Aac
const AAC_SEQ_HDR: u8 = 0;
const AAC_RAW: u8 = 1;

const EX_AUDIO_MARKER: u8 = 0x90; // SoundFormat::ExHeader (9) in the top nibble
const EX_VIDEO_MARKER: u8 = 0x80; // IsExHeader bit

const PT_SEQUENCE_START: u8 = 0;
const PT_CODED_FRAMES: u8 = 1;
const PT_CODED_FRAMES_X: u8 = 3;

const FOURCC_HEVC: [u8; 4] = *b"hvc1";
const FOURCC_AV1: [u8; 4] = *b"av01";
const FOURCC_OPUS: [u8; 4] = *b"Opus";

/// Codecs this module knows how to send over RTMP / Enhanced RTMP.
pub fn supported(codec: Codec) -> bool {
    matches!(codec, Codec::H264 | Codec::H265 | Codec::Av1 | Codec::Aac | Codec::Opus)
}

fn put_i24(b: &mut BytesMut, v: i32) {
    let v = (v & 0x00FF_FFFF) as u32;
    b.put_u8((v >> 16) as u8);
    b.put_u8((v >> 8) as u8);
    b.put_u8(v as u8);
}

fn legacy_video_tag(frame_type: u8, avc_packet_type: u8, cts: i32, payload: &Bytes) -> Bytes {
    let mut b = BytesMut::with_capacity(5 + payload.len());
    b.put_u8((frame_type << 4) | AVC_CODEC_ID);
    b.put_u8(avc_packet_type);
    put_i24(&mut b, cts);
    b.extend_from_slice(payload);
    b.freeze()
}

fn enhanced_video_tag(frame_type: u8, packet_type: u8, fourcc: [u8; 4], payload: &Bytes) -> Bytes {
    let mut b = BytesMut::with_capacity(5 + payload.len());
    b.put_u8(EX_VIDEO_MARKER | (frame_type << 4) | packet_type);
    b.extend_from_slice(&fourcc);
    b.extend_from_slice(payload);
    b.freeze()
}

fn enhanced_video_tag_with_cts(frame_type: u8, fourcc: [u8; 4], cts: i32, payload: &Bytes) -> Bytes {
    let mut b = BytesMut::with_capacity(8 + payload.len());
    b.put_u8(EX_VIDEO_MARKER | (frame_type << 4) | PT_CODED_FRAMES);
    b.extend_from_slice(&fourcc);
    put_i24(&mut b, cts);
    b.extend_from_slice(payload);
    b.freeze()
}

fn legacy_audio_tag(packet_type: u8, payload: &Bytes) -> Bytes {
    let mut b = BytesMut::with_capacity(2 + payload.len());
    // Rate/size/type bits (44kHz/16-bit/stereo) are the encoder's chosen
    // convention, not read by our own ingest demux; the real rate comes
    // from the AudioSpecificConfig sequence header, as with every encoder.
    b.put_u8((AAC_SOUND_FORMAT << 4) | 0b0000_1111);
    b.put_u8(packet_type);
    b.extend_from_slice(payload);
    b.freeze()
}

fn enhanced_audio_tag(packet_type: u8, fourcc: [u8; 4], payload: &Bytes) -> Bytes {
    let mut b = BytesMut::with_capacity(5 + payload.len());
    b.put_u8(EX_AUDIO_MARKER | packet_type);
    b.extend_from_slice(&fourcc);
    b.extend_from_slice(payload);
    b.freeze()
}

/// The sequence header tag for a track, sent once before its first frame
/// (and again after `Event::TracksChanged`). `None` for a codec we don't
/// carry over RTMP.
pub fn sequence_header(info: &TrackInfo) -> Option<Bytes> {
    match info.codec {
        Codec::H264 => Some(legacy_video_tag(1, AVC_SEQ_HDR, 0, &info.init)),
        Codec::H265 => Some(enhanced_video_tag(1, PT_SEQUENCE_START, FOURCC_HEVC, &info.init)),
        Codec::Av1 => Some(enhanced_video_tag(1, PT_SEQUENCE_START, FOURCC_AV1, &info.init)),
        Codec::Aac => Some(legacy_audio_tag(AAC_SEQ_HDR, &info.init)),
        Codec::Opus => Some(enhanced_audio_tag(PT_SEQUENCE_START, FOURCC_OPUS, &info.init)),
        _ => None,
    }
}

/// The tag for one coded frame. `cts_ms` is the composition time offset
/// (pts - dts, in milliseconds), carried for H.264 and HEVC.
pub fn frame_tag(info: &TrackInfo, frame: &Frame, cts_ms: i32) -> Option<Bytes> {
    let frame_type = if frame.keyframe { 1 } else { 2 };
    match info.codec {
        Codec::H264 => Some(legacy_video_tag(frame_type, AVC_NALU, cts_ms, &frame.data)),
        Codec::H265 if cts_ms != 0 => Some(enhanced_video_tag_with_cts(frame_type, FOURCC_HEVC, cts_ms, &frame.data)),
        Codec::H265 => Some(enhanced_video_tag(frame_type, PT_CODED_FRAMES_X, FOURCC_HEVC, &frame.data)),
        Codec::Av1 => Some(enhanced_video_tag(frame_type, PT_CODED_FRAMES_X, FOURCC_AV1, &frame.data)),
        Codec::Aac => Some(legacy_audio_tag(AAC_RAW, &frame.data)),
        Codec::Opus => Some(enhanced_audio_tag(PT_CODED_FRAMES, FOURCC_OPUS, &frame.data)),
        _ => None,
    }
}

/// The `videocodecid` / `audiocodecid` `onMetaData` gives a codec: the
/// legacy FLV id, or the Enhanced RTMP FourCC as a number (what FFmpeg's
/// FLV muxer writes).
fn metadata_codec_id(codec: Codec) -> Option<f64> {
    let fourcc = |f: [u8; 4]| f64::from(u32::from_be_bytes(f));
    match codec {
        Codec::H264 => Some(f64::from(AVC_CODEC_ID)),
        Codec::H265 => Some(fourcc(FOURCC_HEVC)),
        Codec::Av1 => Some(fourcc(FOURCC_AV1)),
        Codec::Aac => Some(f64::from(AAC_SOUND_FORMAT)),
        Codec::Opus => Some(fourcc(FOURCC_OPUS)),
        _ => None,
    }
}

fn put_amf0_key(b: &mut BytesMut, key: &str) {
    b.put_u16(key.len() as u16);
    b.extend_from_slice(key.as_bytes());
}

/// The AMF0 `onMetaData` message for a player: the string `onMetaData`
/// then an object describing `video` and `audio` (either may be absent).
/// Live, so `duration` is 0.
///
/// An AMF0 object (marker 3) rather than the ECMA array (marker 8) FFmpeg's
/// muxer writes; FFmpeg's FLV demuxer reads both, and so does the patched
/// `scuffle-amf0` (see `vendor/README.md`).
pub fn metadata(video: Option<&TrackInfo>, audio: Option<&TrackInfo>) -> Bytes {
    let mut numbers: Vec<(&str, f64)> = vec![("duration", 0.0)];
    let mut stereo = None;
    if let Some(v) = video {
        if let Some(id) = metadata_codec_id(v.codec) {
            numbers.push(("videocodecid", id));
        }
        if let Some(p) = v.video {
            if p.width > 0 && p.height > 0 {
                numbers.push(("width", f64::from(p.width)));
                numbers.push(("height", f64::from(p.height)));
            }
            if let Some(fps) = p.fps {
                numbers.push(("framerate", fps));
            }
        }
    }
    if let Some(a) = audio {
        if let Some(id) = metadata_codec_id(a.codec) {
            numbers.push(("audiocodecid", id));
        }
        if let Some(p) = a.audio {
            numbers.push(("audiosamplerate", f64::from(p.sample_rate)));
            numbers.push(("audiochannels", f64::from(p.channels)));
            stereo = Some(p.channels >= 2);
        }
    }

    let mut b = BytesMut::with_capacity(32 + numbers.len() * 24);
    b.put_u8(0x02); // AMF0 string
    put_amf0_key(&mut b, "onMetaData");
    b.put_u8(0x03); // AMF0 object
    for (key, value) in numbers {
        put_amf0_key(&mut b, key);
        b.put_u8(0x00); // AMF0 number
        b.put_f64(value);
    }
    if let Some(stereo) = stereo {
        put_amf0_key(&mut b, "stereo");
        b.put_u8(0x01); // AMF0 boolean
        b.put_u8(u8::from(stereo));
    }
    b.extend_from_slice(&[0x00, 0x00, 0x09]); // object end
    b.freeze()
}

#[cfg(test)]
mod tests {
    use super::*;
    use caudal_core::{AudioParams, TrackId, VideoParams};

    fn h264() -> TrackInfo {
        TrackInfo {
            id: TrackId(0),
            codec: Codec::H264,
            timescale: 90_000,
            init: Bytes::from_static(b"avcC"),
            lang: None,
            video: Some(VideoParams { width: 1280, height: 720, fps: None }),
            audio: None,
        }
    }

    fn aac() -> TrackInfo {
        TrackInfo {
            id: TrackId(1),
            codec: Codec::Aac,
            timescale: 44_100,
            init: Bytes::from_static(&[0x12, 0x10]),
            lang: None,
            video: None,
            audio: Some(AudioParams { sample_rate: 44_100, channels: 2 }),
        }
    }

    #[test]
    fn h264_sequence_header_matches_legacy_layout() {
        let tag = sequence_header(&h264()).unwrap();
        assert_eq!(tag[0], (1 << 4) | AVC_CODEC_ID);
        assert_eq!(tag[1], AVC_SEQ_HDR);
        assert_eq!(&tag[5..], b"avcC");
    }

    #[test]
    fn h264_frame_carries_composition_time() {
        let frame = Frame { track: TrackId(0), dts: 0, pts: 3000, keyframe: false, data: Bytes::from_static(b"nalu") };
        let tag = frame_tag(&h264(), &frame, 33).unwrap();
        assert_eq!(tag[0], (2 << 4) | AVC_CODEC_ID); // inter frame
        assert_eq!(tag[1], AVC_NALU);
        assert_eq!(&tag[2..5], &[0, 0, 33]);
        assert_eq!(&tag[5..], b"nalu");
    }

    #[test]
    fn aac_sequence_header_is_legacy() {
        let tag = sequence_header(&aac()).unwrap();
        assert_eq!(tag[0] >> 4, AAC_SOUND_FORMAT);
        assert_eq!(tag[1], AAC_SEQ_HDR);
        assert_eq!(&tag[2..], &[0x12, 0x10]);
    }

    #[test]
    fn h265_uses_enhanced_fourcc() {
        let info = TrackInfo { codec: Codec::H265, ..h264() };
        let tag = sequence_header(&info).unwrap();
        assert_eq!(tag[0] & 0x80, 0x80);
        assert_eq!(&tag[1..5], b"hvc1");
    }

    #[test]
    fn hevc_with_reordering_carries_composition_time() {
        let info = TrackInfo { codec: Codec::H265, ..h264() };
        let frame = Frame { track: TrackId(0), dts: 0, pts: 3000, keyframe: true, data: Bytes::from_static(b"nalu") };
        let tag = frame_tag(&info, &frame, 33).unwrap();
        assert_eq!(tag[0], EX_VIDEO_MARKER | (1 << 4) | PT_CODED_FRAMES);
        assert_eq!(&tag[1..5], b"hvc1");
        assert_eq!(&tag[5..8], &[0, 0, 33]);
        assert_eq!(&tag[8..], b"nalu");
        let tag = frame_tag(&info, &frame, 0).unwrap();
        assert_eq!(tag[0] & 0x0f, PT_CODED_FRAMES_X);
        assert_eq!(&tag[5..], b"nalu");
    }

    #[test]
    fn metadata_is_an_object_our_own_parser_reads() {
        let video = TrackInfo { video: Some(VideoParams { width: 1280, height: 720, fps: Some(30.0) }), ..h264() };
        let data = metadata(Some(&video), Some(&aac()));
        assert_eq!(data[0], 0x02);
        assert_eq!(&data[3..13], b"onMetaData");
        assert_eq!(data[13], 0x03);
        assert!(data.ends_with(&[0, 0, 9]));
        assert_eq!(crate::demux::parse_metadata_fps(data), Some(30.0));
    }

    #[test]
    fn unsupported_codecs_are_skipped() {
        assert!(!supported(Codec::Vp8));
        assert!(!supported(Codec::Mp3));
        let info = TrackInfo { codec: Codec::Vp8, ..h264() };
        assert!(sequence_header(&info).is_none());
    }
}
