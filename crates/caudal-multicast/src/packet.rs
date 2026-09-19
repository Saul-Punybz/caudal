//! Datagram framing: groups the muxer's 188-byte TS packets into
//! 7-packet (1316-byte) datagrams, the IPTV convention that keeps each
//! datagram under a 1500-byte Ethernet MTU with room for IP/UDP (and a
//! 12-byte RTP header), and wraps a datagram in an RFC 2250 RTP header
//! when the output format is RTP.

use std::time::Instant;

/// One MPEG-TS packet.
pub(crate) const TS_PACKET: usize = 188;
/// TS packets per datagram.
pub(crate) const PACKETS_PER_DATAGRAM: usize = 7;
/// 7 x 188 = 1316 bytes.
pub(crate) const DATAGRAM_BYTES: usize = TS_PACKET * PACKETS_PER_DATAGRAM;
/// RFC 3551's static payload type for MPEG-2 transport streams (MP2T).
pub(crate) const RTP_PT_MP2T: u8 = 33;
const RTP_HEADER: usize = 12;
/// RFC 2250 §2: the timestamp is a 90 kHz clock.
const RTP_CLOCK_HZ: u128 = 90_000;

/// Accumulates muxed TS bytes and hands out whole datagrams. A remainder
/// shorter than one datagram waits for the next frame's output (tens of
/// milliseconds at most), so every datagram but the last of a session
/// carries exactly seven packets.
#[derive(Default)]
pub(crate) struct Chunker {
    pending: Vec<u8>,
}

impl Chunker {
    /// Appends `ts` (a whole number of TS packets) and moves every full
    /// datagram into `out`.
    pub(crate) fn push(&mut self, ts: &[u8], out: &mut Vec<Vec<u8>>) {
        debug_assert_eq!(ts.len() % TS_PACKET, 0, "the muxer only writes whole packets");
        self.pending.extend_from_slice(ts);
        let full = self.pending.len() / DATAGRAM_BYTES * DATAGRAM_BYTES;
        if full == 0 {
            return;
        }
        out.extend(self.pending[..full].chunks(DATAGRAM_BYTES).map(<[u8]>::to_vec));
        self.pending.drain(..full);
    }

    /// Moves whatever is left (fewer than seven packets) into `out`, as the
    /// session's last, short datagram.
    pub(crate) fn flush(&mut self, out: &mut Vec<Vec<u8>>) {
        if !self.pending.is_empty() {
            out.push(std::mem::take(&mut self.pending));
        }
    }
}

/// RTP sender state for one session: random SSRC and initial sequence
/// number (RFC 3550 §5.1), and a random timestamp origin.
pub(crate) struct RtpState {
    seq: u16,
    ssrc: u32,
    ts_base: u32,
    epoch: Instant,
}

impl RtpState {
    pub(crate) fn new(epoch: Instant) -> Self {
        Self { seq: rand::random(), ssrc: rand::random(), ts_base: rand::random(), epoch }
    }

    #[cfg(test)]
    fn with(seq: u16, ssrc: u32, ts_base: u32, epoch: Instant) -> Self {
        Self { seq, ssrc, ts_base, epoch }
    }

    /// RFC 2250 §2: the 90 kHz timestamp of the target transmission time of
    /// the packet's first byte. `at` is the pacer's scheduled send time.
    fn timestamp(&self, at: Instant) -> u32 {
        let elapsed = at.saturating_duration_since(self.epoch).as_nanos();
        let ticks = (elapsed * RTP_CLOCK_HZ / 1_000_000_000) as u32;
        self.ts_base.wrapping_add(ticks)
    }

    /// Wraps one datagram of TS packets in an RTP header: V=2, no padding,
    /// extension or CSRCs, M=0 (RFC 2250 sets it only on a timestamp
    /// discontinuity, which a paced clock never has), PT 33.
    pub(crate) fn wrap(&mut self, payload: &[u8], at: Instant) -> Vec<u8> {
        let mut buf = Vec::with_capacity(RTP_HEADER + payload.len());
        buf.push(0x80);
        buf.push(RTP_PT_MP2T);
        buf.extend_from_slice(&self.seq.to_be_bytes());
        buf.extend_from_slice(&self.timestamp(at).to_be_bytes());
        buf.extend_from_slice(&self.ssrc.to_be_bytes());
        buf.extend_from_slice(payload);
        self.seq = self.seq.wrapping_add(1);
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn packets(n: usize) -> Vec<u8> {
        let mut v = Vec::new();
        for i in 0..n {
            let mut p = [0xFFu8; TS_PACKET];
            p[0] = 0x47;
            p[3] = i as u8;
            v.extend_from_slice(&p);
        }
        v
    }

    #[test]
    fn datagrams_are_seven_packets() {
        assert_eq!(DATAGRAM_BYTES, 1316);
        let mut c = Chunker::default();
        let mut out = Vec::new();
        c.push(&packets(20), &mut out);
        assert_eq!(out.len(), 2, "20 packets = two full datagrams + 6 pending");
        assert!(out.iter().all(|d| d.len() == 1316));
        assert!(out.iter().all(|d| d.chunks(TS_PACKET).all(|p| p[0] == 0x47)), "packet aligned");
        c.push(&packets(1), &mut out);
        assert_eq!(out.len(), 3, "the 7th packet completes the third datagram");
        c.flush(&mut out);
        assert_eq!(out.len(), 3, "nothing left to flush");
    }

    #[test]
    fn flush_emits_a_short_last_datagram() {
        let mut c = Chunker::default();
        let mut out = Vec::new();
        c.push(&packets(3), &mut out);
        assert!(out.is_empty());
        c.flush(&mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].len(), 3 * TS_PACKET);
    }

    #[test]
    fn packet_order_is_preserved_across_pushes() {
        let mut c = Chunker::default();
        let mut out = Vec::new();
        let all = packets(14);
        c.push(&all[..5 * TS_PACKET], &mut out);
        c.push(&all[5 * TS_PACKET..], &mut out);
        assert_eq!(out.concat(), all);
    }

    #[test]
    fn rtp_header_is_rfc2250() {
        let epoch = Instant::now();
        let mut rtp = RtpState::with(0xFFFF, 0xDEADBEEF, 1000, epoch);
        let payload = vec![0x47; DATAGRAM_BYTES];
        let a = rtp.wrap(&payload, epoch);
        assert_eq!(a.len(), 12 + 1316);
        assert_eq!(a[0], 0x80, "V=2, P=0, X=0, CC=0");
        assert_eq!(a[1], 33, "M=0, PT=33 (MP2T)");
        assert_eq!(u16::from_be_bytes([a[2], a[3]]), 0xFFFF);
        assert_eq!(u32::from_be_bytes([a[4], a[5], a[6], a[7]]), 1000);
        assert_eq!(u32::from_be_bytes([a[8], a[9], a[10], a[11]]), 0xDEADBEEF);
        assert_eq!(&a[12..], &payload[..]);

        // One second later: +90000 ticks, and the sequence number wraps.
        let b = rtp.wrap(&payload, epoch + Duration::from_secs(1));
        assert_eq!(u16::from_be_bytes([b[2], b[3]]), 0);
        assert_eq!(u32::from_be_bytes([b[4], b[5], b[6], b[7]]), 91_000);
        // 10 ms = 900 ticks.
        let c = rtp.wrap(&payload, epoch + Duration::from_millis(1010));
        assert_eq!(u32::from_be_bytes([c[4], c[5], c[6], c[7]]), 91_900);
    }

    #[test]
    fn rtp_timestamp_wraps_at_32_bits() {
        let epoch = Instant::now();
        let mut rtp = RtpState::with(0, 1, u32::MAX, epoch);
        let p = rtp.wrap(&[0x47; 188], epoch + Duration::from_micros(12));
        assert_eq!(u32::from_be_bytes([p[4], p[5], p[6], p[7]]), 0, "u32::MAX + 1 tick wraps to 0");
    }
}
