//! RTP packetizing: H.264 FU-A (RFC 6184) / H.265 FU (RFC 7798) for video,
//! RFC 3640 AU headers (`mode=AAC-hbr`) for AAC. Hand-rolled rather than
//! pulled from `webrtc-rs`'s `rtp` crate: its `H264Payloader` expects
//! Annex B input, while `caudal-core::Frame` is always AVCC, so writing the
//! (small, well-specified) fragmenter directly avoids a lossy round trip.
//!
//! [`packetize`] writes one access unit's RTP packets into a reusable
//! [`Packets`] buffer, each already wrapped in RFC 2326 §10.12's `$` +
//! channel + 16-bit length. TCP interleaved sends the whole buffer with one
//! write ([`Packets::framed`]); UDP sends each bare packet (the same bytes
//! minus the 4-byte prefix, [`Packets::bare`]) as its own datagram.
//! Packetizing straight into one buffer, reused across frames, avoids three
//! heap allocations per RTP packet and, on TCP, one write syscall per RTP
//! packet: this runs once per frame per viewer, the RTSP fan-out hot path.

use caudal_core::{Codec, Frame, TrackInfo};

pub(crate) const VIDEO_PT: u8 = 96;
pub(crate) const AUDIO_PT: u8 = 97;

/// A conservative payload size for one RTP packet's fragment, safely under
/// the 16-bit interleaved frame length and typical path MTUs.
const MTU: usize = 1200;

/// Per-track RTP state: sequence number and SSRC (both random per session),
/// plus the running totals RTCP Sender Reports need.
pub(crate) struct PacketState {
    seq: u16,
    ssrc: u32,
    packet_count: u32,
    octet_count: u32,
    last_ts: u32,
}

impl PacketState {
    pub(crate) fn new() -> Self {
        Self { seq: rand::random(), ssrc: rand::random(), packet_count: 0, octet_count: 0, last_ts: 0 }
    }

    pub(crate) fn ssrc(&self) -> u32 {
        self.ssrc
    }

    pub(crate) fn packet_count(&self) -> u32 {
        self.packet_count
    }

    pub(crate) fn octet_count(&self) -> u32 {
        self.octet_count
    }

    pub(crate) fn last_ts(&self) -> u32 {
        self.last_ts
    }
}

/// One access unit's RTP packets, interleave-framed back to back in one
/// buffer. Reused across frames ([`packetize`] clears it), so steady-state
/// packetizing allocates nothing.
#[derive(Default)]
pub(crate) struct Packets {
    buf: Vec<u8>,
    /// Offset of each packet's 4-byte interleaved prefix in `buf`.
    starts: Vec<usize>,
}

impl Packets {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Every packet with its TCP interleaved prefix, back to back: what a
    /// TCP (or TLS) connection writes in one go.
    pub(crate) fn framed(&self) -> &[u8] {
        &self.buf
    }

    /// Drops an oversized buffer (a keyframe's) so an idle-between-keyframes
    /// viewer doesn't hold a keyframe's worth of memory: with hundreds of
    /// viewers that adds up, while reallocating once per keyframe is cheap.
    pub(crate) fn trim(&mut self) {
        const KEEP: usize = 64 * 1024;
        if self.buf.capacity() > KEEP {
            self.buf = Vec::new();
        }
    }

    /// Each bare RTP packet (header + payload, no framing), for UDP.
    pub(crate) fn bare(&self) -> impl Iterator<Item = &[u8]> + '_ {
        self.starts.iter().enumerate().map(move |(i, &s)| {
            let end = self.starts.get(i + 1).copied().unwrap_or(self.buf.len());
            &self.buf[s + 4..end]
        })
    }

    /// Appends one packet: interleaved prefix, 12-byte RTP header, then
    /// `parts` (the payload, possibly split to avoid an intermediate copy).
    fn push(&mut self, channel: u8, pt: u8, state: &mut PacketState, ts: u32, marker: bool, parts: &[&[u8]]) {
        let payload_len: usize = parts.iter().map(|p| p.len()).sum();
        let rtp_len = 12 + payload_len;
        let seq = state.seq;
        state.seq = state.seq.wrapping_add(1);
        self.starts.push(self.buf.len());
        self.buf.reserve(4 + rtp_len);
        self.buf.extend_from_slice(&[b'$', channel]);
        self.buf.extend_from_slice(&(rtp_len as u16).to_be_bytes());
        self.buf.extend_from_slice(&[0x80, (u8::from(marker) << 7) | (pt & 0x7F)]); // V=2, P=0, X=0, CC=0
        self.buf.extend_from_slice(&seq.to_be_bytes());
        self.buf.extend_from_slice(&ts.to_be_bytes());
        self.buf.extend_from_slice(&state.ssrc.to_be_bytes());
        for p in parts {
            self.buf.extend_from_slice(p);
        }
        state.packet_count = state.packet_count.wrapping_add(1);
        state.octet_count = state.octet_count.wrapping_add(payload_len as u32);
        state.last_ts = ts;
    }
}

/// Splits AVCC (4-byte length-prefixed) NAL units.
fn avcc_nalus(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = data;
    std::iter::from_fn(move || {
        if rest.len() < 4 {
            return None;
        }
        let len = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
        if rest.len() - 4 < len {
            return None;
        }
        let nalu = &rest[4..4 + len];
        rest = &rest[4 + len..];
        Some(nalu)
    })
}

/// Packetizes one access unit into `out` (cleared first), each packet
/// framed for TCP interleaved `channel` (UDP ignores it and sends
/// [`Packets::bare`]). Unsupported codecs produce no packets (never
/// panics).
pub(crate) fn packetize(info: &TrackInfo, frame: &Frame, state: &mut PacketState, channel: u8, out: &mut Packets) {
    out.buf.clear();
    out.starts.clear();
    let ts = frame.pts as u32;
    match info.codec {
        Codec::H264 => packetize_h264(&frame.data, state, ts, channel, out),
        Codec::H265 => packetize_h265(&frame.data, state, ts, channel, out),
        Codec::Aac => packetize_aac(&frame.data, state, ts, channel, out),
        _ => {}
    }
}

fn packetize_h264(data: &[u8], state: &mut PacketState, ts: u32, channel: u8, out: &mut Packets) {
    let mut nalus = avcc_nalus(data).peekable();
    while let Some(nalu) = nalus.next() {
        let last_nalu = nalus.peek().is_none();
        if nalu.is_empty() {
            continue;
        }
        if nalu.len() <= MTU {
            out.push(channel, VIDEO_PT, state, ts, last_nalu, &[nalu]);
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
            let mut hdr = nal_type;
            if first {
                hdr |= 0x80;
            }
            if last {
                hdr |= 0x40;
            }
            out.push(channel, VIDEO_PT, state, ts, last_nalu && last, &[&[fnri | 28, hdr], &payload[off..end]]);
            off = end;
        }
    }
}

/// RFC 7798 §4.4.3 FU, for the H.265 tracks `retina` can hand us on pull.
/// Best-effort: not exercised by the H.264 test suite.
fn packetize_h265(data: &[u8], state: &mut PacketState, ts: u32, channel: u8, out: &mut Packets) {
    let mut nalus = avcc_nalus(data).peekable();
    while let Some(nalu) = nalus.next() {
        let last_nalu = nalus.peek().is_none();
        if nalu.len() < 2 {
            continue;
        }
        if nalu.len() <= MTU {
            out.push(channel, VIDEO_PT, state, ts, last_nalu, &[nalu]);
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
            let mut hdr = nal_type;
            if first {
                hdr |= 0x80;
            }
            if last {
                hdr |= 0x40;
            }
            let fu = [(49 << 1) | layer_id_high, layer_id_low_tid, hdr];
            out.push(channel, VIDEO_PT, state, ts, last_nalu && last, &[&fu, &payload[off..end]]);
            off = end;
        }
    }
}

/// RFC 3640 `mode=AAC-hbr`: one 16-bit AU-headers-length, one 16-bit
/// AU-header (13-bit size, 3-bit index-delta = 0), then the raw AU. AAC
/// frames are always well under the MTU, so no fragmentation is needed.
fn packetize_aac(data: &[u8], state: &mut PacketState, ts: u32, channel: u8, out: &mut Packets) {
    let size = (data.len() as u16) & 0x1FFF;
    let au = (size << 3).to_be_bytes();
    out.push(channel, AUDIO_PT, state, ts, true, &[&16u16.to_be_bytes(), &au, data]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use caudal_core::TrackId;

    fn track(id: u32, codec: Codec, timescale: u32) -> TrackInfo {
        TrackInfo { id: TrackId(id), codec, timescale, init: Bytes::new(), lang: None, video: None, audio: None }
    }

    fn avcc(nalus: &[&[u8]]) -> Bytes {
        let mut v = Vec::new();
        for n in nalus {
            v.extend_from_slice(&(n.len() as u32).to_be_bytes());
            v.extend_from_slice(n);
        }
        Bytes::from(v)
    }

    fn frame(data: Bytes) -> Frame {
        Frame { track: TrackId(0), dts: 3000, pts: 3000, keyframe: true, data }
    }

    /// Walks the interleaved buffer and checks every prefix against the
    /// bare packets, so TCP and UDP carry exactly the same RTP bytes.
    fn check_framing(out: &Packets, channel: u8) -> Vec<Vec<u8>> {
        let mut rest = out.framed();
        let mut from_tcp = Vec::new();
        while !rest.is_empty() {
            assert_eq!(rest[0], b'$');
            assert_eq!(rest[1], channel);
            let len = u16::from_be_bytes([rest[2], rest[3]]) as usize;
            from_tcp.push(rest[4..4 + len].to_vec());
            rest = &rest[4 + len..];
        }
        let from_udp: Vec<Vec<u8>> = out.bare().map(<[u8]>::to_vec).collect();
        assert_eq!(from_tcp, from_udp);
        from_udp
    }

    #[test]
    fn small_nalus_are_single_packets_with_marker_on_the_last() {
        let mut st = PacketState::new();
        let seq0 = st.seq;
        let mut out = Packets::new();
        let data = avcc(&[&[0x67, 1, 2], &[0x68, 3], &[0x65, 4, 5, 6]]);
        packetize(&track(0, Codec::H264, 90_000), &frame(data), &mut st, 2, &mut out);
        let pkts = check_framing(&out, 2);
        assert_eq!(pkts.len(), 3);
        for (i, p) in pkts.iter().enumerate() {
            assert_eq!(p[0], 0x80);
            assert_eq!(p[1] & 0x80 != 0, i == 2, "marker only on the last packet");
            assert_eq!(p[1] & 0x7F, VIDEO_PT);
            assert_eq!(u16::from_be_bytes([p[2], p[3]]), seq0.wrapping_add(i as u16));
            assert_eq!(u32::from_be_bytes([p[4], p[5], p[6], p[7]]), 3000);
            assert_eq!(u32::from_be_bytes([p[8], p[9], p[10], p[11]]), st.ssrc());
        }
        assert_eq!(&pkts[2][12..], &[0x65, 4, 5, 6]);
        assert_eq!(st.packet_count(), 3);
        assert_eq!(st.octet_count(), 3 + 2 + 4);
        assert_eq!(st.last_ts(), 3000);
    }

    #[test]
    fn fu_a_fragments_reassemble_to_the_original_nalu() {
        let mut nalu = vec![0x65u8];
        nalu.extend((0..5000u32).map(|i| (i * 7) as u8));
        let mut st = PacketState::new();
        let mut out = Packets::new();
        packetize(&track(0, Codec::H264, 90_000), &frame(avcc(&[&nalu])), &mut st, 0, &mut out);
        let pkts = check_framing(&out, 0);
        assert!(pkts.len() > 1);
        let mut rebuilt = vec![(pkts[0][12] & 0xE0) | (pkts[0][13] & 0x1F)];
        for (i, p) in pkts.iter().enumerate() {
            assert!(p.len() - 12 <= MTU);
            assert_eq!(p[12] & 0x1F, 28, "FU-A indicator");
            assert_eq!(p[13] & 0x80 != 0, i == 0, "start bit");
            assert_eq!(p[13] & 0x40 != 0, i + 1 == pkts.len(), "end bit");
            assert_eq!(p[1] & 0x80 != 0, i + 1 == pkts.len(), "marker");
            rebuilt.extend_from_slice(&p[14..]);
        }
        assert_eq!(rebuilt, nalu);
    }

    #[test]
    fn h265_fu_reassembles_to_the_original_nalu() {
        let mut nalu = vec![19 << 1, 1];
        nalu.extend((0..3000u32).map(|i| (i * 13) as u8));
        let mut st = PacketState::new();
        let mut out = Packets::new();
        packetize(&track(0, Codec::H265, 90_000), &frame(avcc(&[&nalu])), &mut st, 0, &mut out);
        let pkts = check_framing(&out, 0);
        let mut rebuilt = vec![(pkts[0][14] & 0x3F) << 1, pkts[0][13]];
        for p in &pkts {
            assert_eq!(p[12] >> 1, 49, "FU type");
            rebuilt.extend_from_slice(&p[15..]);
        }
        assert_eq!(rebuilt, nalu);
    }

    #[test]
    fn aac_is_one_rfc3640_packet_and_the_buffer_is_reused() {
        let aac = track(1, Codec::Aac, 48_000);
        let mut st = PacketState::new();
        let mut out = Packets::new();
        let au = [9u8; 300];
        for _ in 0..2 {
            packetize(&aac, &frame(Bytes::copy_from_slice(&au)), &mut st, 3, &mut out);
            let pkts = check_framing(&out, 3);
            assert_eq!(pkts.len(), 1);
            let p = &pkts[0];
            assert_eq!(p[1], 0x80 | AUDIO_PT);
            assert_eq!(&p[12..14], &16u16.to_be_bytes());
            assert_eq!(u16::from_be_bytes([p[14], p[15]]) >> 3, 300);
            assert_eq!(&p[16..], &au);
        }
        assert_eq!(st.packet_count(), 2);
    }

    #[test]
    fn truncated_avcc_stops_without_panicking() {
        let mut data = avcc(&[&[0x65, 1, 2, 3]]).to_vec();
        data.extend_from_slice(&[0, 0, 0xFF, 0xFF, 1]);
        let mut st = PacketState::new();
        let mut out = Packets::new();
        packetize(&track(0, Codec::H264, 90_000), &frame(Bytes::from(data)), &mut st, 0, &mut out);
        assert_eq!(check_framing(&out, 0).len(), 1);
    }
}
