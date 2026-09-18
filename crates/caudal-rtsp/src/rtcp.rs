//! Minimal RTCP for UDP unicast playback: a Sender Report (RFC 3550
//! §6.4.1) plus an SDES CNAME chunk (§6.5) as one compound packet, sent
//! periodically so players compute A/V sync and can tell the session is
//! alive. Nothing here parses incoming Receiver Reports: their mere
//! arrival on our RTCP socket is treated as a keep-alive signal (see
//! `server.rs`), same as gortsplib and most minimal RTSP servers do.
//!
//! Hand-rolled rather than pulling in the `rtcp` crate (webrtc-rs): a
//! compound SR+SDES packet is ~40 bytes of fixed-layout fields, smaller
//! than the dependency, and this crate already hand-rolls RTP the same way
//! (see `rtp.rs`).

const NTP_UNIX_EPOCH_OFFSET: u64 = 2_208_988_800; // 1900-01-01 -> 1970-01-01, in seconds

/// The current time as an RFC 3550 §4 64-bit NTP timestamp
/// (seconds-since-1900, fractional-seconds).
pub(crate) fn ntp_now() -> (u32, u32) {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let secs = now.as_secs() + NTP_UNIX_EPOCH_OFFSET;
    let frac = ((u64::from(now.subsec_nanos())) << 32) / 1_000_000_000;
    (secs as u32, frac as u32)
}

/// Builds a compound RTCP packet: Sender Report (no report blocks; we never
/// received RTP from the player) followed by an SDES CNAME chunk.
pub(crate) fn sender_report(
    ssrc: u32,
    ntp: (u32, u32),
    rtp_ts: u32,
    packet_count: u32,
    octet_count: u32,
    cname: &str,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(28 + 12 + cname.len());

    // SR: header (4 bytes) + SSRC + NTP MSW/LSW + RTP ts + packet/octet
    // counts = 28 bytes = 7 words; length = words - 1.
    buf.push(0x80); // V=2, P=0, RC=0 (no report blocks)
    buf.push(200); // PT=SR
    buf.extend_from_slice(&6u16.to_be_bytes());
    buf.extend_from_slice(&ssrc.to_be_bytes());
    buf.extend_from_slice(&ntp.0.to_be_bytes());
    buf.extend_from_slice(&ntp.1.to_be_bytes());
    buf.extend_from_slice(&rtp_ts.to_be_bytes());
    buf.extend_from_slice(&packet_count.to_be_bytes());
    buf.extend_from_slice(&octet_count.to_be_bytes());

    // SDES: one chunk (SSRC + CNAME item + null terminator), padded to a
    // 32-bit boundary.
    let cname_bytes = &cname.as_bytes()[..cname.len().min(255)];
    let mut chunk = Vec::with_capacity(4 + 2 + cname_bytes.len() + 1);
    chunk.extend_from_slice(&ssrc.to_be_bytes());
    chunk.push(1); // CNAME
    chunk.push(cname_bytes.len() as u8);
    chunk.extend_from_slice(cname_bytes);
    chunk.push(0); // item-list terminator
    while chunk.len() % 4 != 0 {
        chunk.push(0);
    }
    let sdes_length_words = (chunk.len() / 4) as u16; // (4-byte header + chunk) / 4 - 1 == chunk.len() / 4
    buf.push(0x81); // V=2, P=0, SC=1
    buf.push(202); // PT=SDES
    buf.extend_from_slice(&sdes_length_words.to_be_bytes());
    buf.extend_from_slice(&chunk);

    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sender_report_is_well_formed() {
        let pkt = sender_report(0x1234_5678, (1_000, 2_000), 90_000, 42, 12_345, "caudal-0");
        // SR header
        assert_eq!(pkt[0], 0x80);
        assert_eq!(pkt[1], 200);
        let sr_len = u16::from_be_bytes([pkt[2], pkt[3]]);
        assert_eq!(sr_len, 6);
        assert_eq!(&pkt[4..8], &0x1234_5678u32.to_be_bytes());
        assert_eq!(&pkt[8..12], &1_000u32.to_be_bytes());
        assert_eq!(&pkt[12..16], &2_000u32.to_be_bytes());
        assert_eq!(&pkt[16..20], &90_000u32.to_be_bytes());
        assert_eq!(&pkt[20..24], &42u32.to_be_bytes());
        assert_eq!(&pkt[24..28], &12_345u32.to_be_bytes());

        // SDES follows immediately, its length matches the actual body.
        let sdes = &pkt[28..];
        assert_eq!(sdes[0], 0x81);
        assert_eq!(sdes[1], 202);
        let sdes_len_words = u16::from_be_bytes([sdes[2], sdes[3]]) as usize;
        assert_eq!(sdes.len(), 4 + sdes_len_words * 4);
        assert_eq!(pkt.len() % 4, 0, "compound packet should be 32-bit aligned");
    }

    #[test]
    fn ntp_now_is_after_the_ntp_epoch_and_close_to_unix_now() {
        let (secs, _frac) = ntp_now();
        let unix_now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as u32;
        let expected = unix_now.wrapping_add(NTP_UNIX_EPOCH_OFFSET as u32);
        assert!(secs.abs_diff(expected) <= 2);
    }
}
