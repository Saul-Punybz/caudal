//! Builds RTMP `VIDEODATA`/`AUDIODATA` message payloads (FLV tag bodies) for
//! outbound frames.
//!
//! Byte layouts match what `crates/caudal-rtmp/src/demux.rs` reads on the
//! way in (legacy AVC/AAC tags per the FLV spec, Enhanced RTMP v2 FourCC
//! tags for HEVC/AV1/Opus per <https://github.com/veovera/enhanced-rtmp>),
//! confirmed against `scuffle-flv`'s demux source rather than assumed. The
//! loopback test round-trips through `caudal-rtmp` itself, so a byte
//! mismatch here fails a test, not a real push.
//!
//! Enhanced video frames are always sent as `CodedFramesX` (packet type 3):
//! no composition-time field, so HEVC/AV1 restreamed through this crate
//! carry the frame's `pts` as the RTMP timestamp and lose dts/pts
//! reordering fidelity for B-frames. Legacy H.264 keeps full fidelity via
//! the AVC packet's 24-bit composition time offset.

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

/// Codecs this crate knows how to push over RTMP / Enhanced RTMP.
pub(crate) fn supported(codec: Codec) -> bool {
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
pub(crate) fn sequence_header(info: &TrackInfo) -> Option<Bytes> {
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
/// (pts - dts, in milliseconds); only legacy H.264 carries it on the wire.
pub(crate) fn frame_tag(info: &TrackInfo, frame: &Frame, cts_ms: i32) -> Option<Bytes> {
    let frame_type = if frame.keyframe { 1 } else { 2 };
    match info.codec {
        Codec::H264 => Some(legacy_video_tag(frame_type, AVC_NALU, cts_ms, &frame.data)),
        Codec::H265 => Some(enhanced_video_tag(frame_type, PT_CODED_FRAMES_X, FOURCC_HEVC, &frame.data)),
        Codec::Av1 => Some(enhanced_video_tag(frame_type, PT_CODED_FRAMES_X, FOURCC_AV1, &frame.data)),
        Codec::Aac => Some(legacy_audio_tag(AAC_RAW, &frame.data)),
        Codec::Opus => Some(enhanced_audio_tag(PT_CODED_FRAMES, FOURCC_OPUS, &frame.data)),
        _ => None,
    }
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
    fn unsupported_codecs_are_skipped() {
        assert!(!supported(Codec::Vp8));
        assert!(!supported(Codec::Mp3));
        let info = TrackInfo { codec: Codec::Vp8, ..h264() };
        assert!(sequence_header(&info).is_none());
    }
}
