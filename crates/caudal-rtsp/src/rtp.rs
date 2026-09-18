//! RTP packetizing over TCP interleaved framing (`$` + channel + 16-bit
//! length, RFC 2326 §10.12): H.264 FU-A (RFC 6184) / H.265 FU (RFC 7798) for
//! video, RFC 3640 AU headers (`mode=AAC-hbr`) for AAC. Hand-rolled rather
//! than pulled from `webrtc-rs`'s `rtp` crate: its `H264Payloader` expects
//! Annex B input, while `caudal-core::Frame` is always AVCC, so writing the
//! (small, well-specified) fragmenter directly avoids a lossy round trip.

use caudal_core::{Codec, Frame, TrackInfo};

pub(crate) const VIDEO_PT: u8 = 96;
pub(crate) const AUDIO_PT: u8 = 97;

/// A conservative payload size for one RTP packet's fragment, safely under
/// the 16-bit interleaved frame length and typical path MTUs.
const MTU: usize = 1200;

/// Per-track RTP state: sequence number and SSRC, both random per session.
pub(crate) struct PacketState {
    seq: u16,
    ssrc: u32,
}

impl PacketState {
    pub(crate) fn new() -> Self {
        Self { seq: rand::random(), ssrc: rand::random() }
    }
}

/// Splits AVCC (4-byte length-prefixed) NAL units.
fn avcc_nalus(data: &[u8]) -> Vec<&[u8]> {
    let mut nalus = Vec::new();
    let mut rest = data;
    while rest.len() >= 4 {
        let len = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
        if rest.len() < 4 + len {
            break;
        }
        nalus.push(&rest[4..4 + len]);
        rest = &rest[4 + len..];
    }
    nalus
}

fn rtp_packet(channel: u8, pt: u8, state: &mut PacketState, ts: u32, marker: bool, payload: &[u8]) -> Vec<u8> {
    let seq = state.seq;
    state.seq = state.seq.wrapping_add(1);
    let mut buf = Vec::with_capacity(4 + 12 + payload.len());
    buf.push(b'$');
    buf.push(channel);
    buf.extend_from_slice(&((12 + payload.len()) as u16).to_be_bytes());
    buf.push(0x80); // V=2, P=0, X=0, CC=0
    buf.push((u8::from(marker) << 7) | (pt & 0x7F));
    buf.extend_from_slice(&seq.to_be_bytes());
    buf.extend_from_slice(&ts.to_be_bytes());
    buf.extend_from_slice(&state.ssrc.to_be_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// Packetizes one access unit into wire-ready interleaved frames.
/// Unsupported codecs produce no packets (never panics).
pub(crate) fn packetize(info: &TrackInfo, frame: &Frame, state: &mut PacketState, channel: u8) -> Vec<Vec<u8>> {
    let ts = frame.pts as u32;
    match info.codec {
        Codec::H264 => packetize_h264(&frame.data, state, channel, ts),
        Codec::H265 => packetize_h265(&frame.data, state, channel, ts),
        Codec::Aac => vec![packetize_aac(&frame.data, state, channel, ts)],
        _ => Vec::new(),
    }
}

fn packetize_h264(data: &[u8], state: &mut PacketState, channel: u8, ts: u32) -> Vec<Vec<u8>> {
    let nalus = avcc_nalus(data);
    let n = nalus.len();
    let mut out = Vec::new();
    for (i, nalu) in nalus.into_iter().enumerate() {
        if nalu.is_empty() {
            continue;
        }
        let last_nalu = i + 1 == n;
        if nalu.len() <= MTU {
            out.push(rtp_packet(channel, VIDEO_PT, state, ts, last_nalu, nalu));
            continue;
        }
        // FU-A (RFC 6184 §5.8).
        let fnri = nalu[0] & 0xE0;
        let nal_type = nalu[0] & 0x1F;
        let payload = &nalu[1..];
        let chunk = MTU - 2;
        let mut off = 0;
        while off < payload.len() {
            let end = (off + chunk).min(payload.len());
            let first = off == 0;
            let last = end == payload.len();
            let mut fu = Vec::with_capacity(2 + (end - off));
            fu.push(fnri | 28);
            let mut hdr = nal_type;
            if first {
                hdr |= 0x80;
            }
            if last {
                hdr |= 0x40;
            }
            fu.push(hdr);
            fu.extend_from_slice(&payload[off..end]);
            out.push(rtp_packet(channel, VIDEO_PT, state, ts, last_nalu && last, &fu));
            off = end;
        }
    }
    out
}

/// RFC 7798 §4.4.3 FU, for the H.265 tracks `retina` can hand us on pull.
/// Best-effort: not exercised by the H.264 test suite.
fn packetize_h265(data: &[u8], state: &mut PacketState, channel: u8, ts: u32) -> Vec<Vec<u8>> {
    let nalus = avcc_nalus(data);
    let n = nalus.len();
    let mut out = Vec::new();
    for (i, nalu) in nalus.into_iter().enumerate() {
        if nalu.len() < 2 {
            continue;
        }
        let last_nalu = i + 1 == n;
        if nalu.len() <= MTU {
            out.push(rtp_packet(channel, VIDEO_PT, state, ts, last_nalu, nalu));
            continue;
        }
        let nal_type = (nalu[0] >> 1) & 0x3F;
        let layer_id_high = nalu[0] & 0x01;
        let layer_id_low_tid = nalu[1];
        let payload = &nalu[2..];
        let chunk = MTU - 3;
        let mut off = 0;
        while off < payload.len() {
            let end = (off + chunk).min(payload.len());
            let first = off == 0;
            let last = end == payload.len();
            let mut fu = Vec::with_capacity(3 + (end - off));
            fu.push((49 << 1) | layer_id_high);
            fu.push(layer_id_low_tid);
            let mut hdr = nal_type;
            if first {
                hdr |= 0x80;
            }
            if last {
                hdr |= 0x40;
            }
            fu.push(hdr);
            fu.extend_from_slice(&payload[off..end]);
            out.push(rtp_packet(channel, VIDEO_PT, state, ts, last_nalu && last, &fu));
            off = end;
        }
    }
    out
}

/// RFC 3640 `mode=AAC-hbr`: one 16-bit AU-headers-length, one 16-bit
/// AU-header (13-bit size, 3-bit index-delta = 0), then the raw AU. AAC
/// frames are always well under the MTU, so no fragmentation is needed.
fn packetize_aac(data: &[u8], state: &mut PacketState, channel: u8, ts: u32) -> Vec<u8> {
    let size = (data.len() as u16) & 0x1FFF;
    let mut payload = Vec::with_capacity(4 + data.len());
    payload.extend_from_slice(&16u16.to_be_bytes());
    payload.extend_from_slice(&(size << 3).to_be_bytes());
    payload.extend_from_slice(data);
    rtp_packet(channel, AUDIO_PT, state, ts, true, &payload)
}
