//! Raw MPEG-TS demux: turns TS packets carried in SRT payloads into
//! elementary-stream access units (PES payload + PTS/DTS), using `mpeg2ts`
//! for TS/PSI/PES parsing (see `NOTES.md` for why `mpeg2ts` over
//! `moq-mux`).
//!
//! `mpeg2ts::ts::TsPacketReader` reads packets from a `std::io::Read`, so
//! the bytes an SRT connection receives are pushed into a `SharedQueue`
//! that the reader pulls from. Packets are only parsed 188 bytes at a time
//! and only once that much is buffered, so `Read::read` never has to
//! return 0 for "no data yet" — which `TsPacketReader` would otherwise take
//! as end of stream, ending demux for the rest of the connection.

use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::sync::Arc;

use mpeg2ts::es::StreamType;
use mpeg2ts::pes::PesHeader;
use mpeg2ts::ts::payload::{Pes, Pmt};
use mpeg2ts::ts::{Pid, ReadTsPacket, TsPacketReader, TsPayload};
use parking_lot::Mutex;

const TS_PACKET_SIZE: usize = 188;

#[derive(Clone)]
struct SharedQueue(Arc<Mutex<VecDeque<u8>>>);

impl SharedQueue {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(VecDeque::new())))
    }

    fn push(&self, data: &[u8]) {
        self.0.lock().extend(data.iter().copied());
    }

    fn len(&self) -> usize {
        self.0.lock().len()
    }

    /// PID of the TS packet at the front, if one is buffered and in sync.
    fn front_pid(&self) -> Option<u16> {
        let q = self.0.lock();
        (q.len() >= TS_PACKET_SIZE && q[0] == 0x47).then(|| u16::from_be_bytes([q[1], q[2]]) & 0x1FFF)
    }

    fn pop_packet(&self) -> [u8; TS_PACKET_SIZE] {
        let mut out = [0; TS_PACKET_SIZE];
        let mut q = self.0.lock();
        for slot in out.iter_mut() {
            *slot = q.pop_front().expect("caller checked a whole packet is buffered");
        }
        out
    }
}

impl Read for SharedQueue {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let mut q = self.0.lock();
        let n = out.len().min(q.len());
        for slot in out.iter_mut().take(n) {
            *slot = q.pop_front().expect("length checked above");
        }
        Ok(n)
    }
}

/// The codecs this ingest understands. Any other `StreamType` in the PMT is
/// ignored (its PID's PES packets are dropped, never handed to the caller).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EsKind {
    H264,
    H265,
    Aac,
    /// An SCTE-35 `splice_info_section` (stream_type 0x86). Carried as PSI
    /// sections, not PES: `data` is one whole section and `pts`/`dts` are
    /// `None` (the splice time lives inside the section).
    Scte35,
}

/// SCTE 35 §8.1. `mpeg2ts` names 0x86 after its Blu-ray meaning (DTS-HD
/// lossless audio); in broadcast TS it is the SCTE-35 cue PID.
const STREAM_TYPE_SCTE35: u8 = 0x86;

fn es_kind(stream_type: StreamType) -> Option<EsKind> {
    match stream_type {
        StreamType::H264 => Some(EsKind::H264),
        StreamType::H265 => Some(EsKind::H265),
        StreamType::AdtsAac => Some(EsKind::Aac),
        st if st as u8 == STREAM_TYPE_SCTE35 => Some(EsKind::Scte35),
        _ => None,
    }
}

/// One reassembled PES packet (or, for [`EsKind::Scte35`], one section):
/// raw elementary-stream payload plus its wire-clock (33-bit, 90 kHz)
/// timestamps, not yet unwrapped or rebased.
pub struct EsUnit {
    pub kind: EsKind,
    pub pts: Option<u64>,
    pub dts: Option<u64>,
    pub data: Vec<u8>,
}

/// Mirrors `PesHeader::optional_header_len` (private in `mpeg2ts`): the
/// fixed 3 flag/length bytes plus whichever timestamp/ESCR fields are
/// present. ISO/IEC 13818-1 2.4.3.7.
fn optional_header_len(header: &PesHeader) -> u16 {
    3 + header.pts.map_or(0, |_| 5) + header.dts.map_or(0, |_| 5) + header.escr.map_or(0, |_| 6)
}

struct Partial {
    kind: EsKind,
    pts: Option<u64>,
    dts: Option<u64>,
    data: Vec<u8>,
    /// Total ES payload length the PES header declared, if any. `None`
    /// covers both "not declared" (`pes_packet_len == 0`, the norm for
    /// video) and "declared as zero", which are indistinguishable on the
    /// wire and treated the same way: the next `PesStart` on this PID ends
    /// the packet.
    expected_len: Option<usize>,
}

impl Partial {
    fn finish(self) -> EsUnit {
        EsUnit { kind: self.kind, pts: self.pts, dts: self.dts, data: self.data }
    }

    fn is_complete(&self) -> bool {
        self.expected_len.is_some_and(|len| self.data.len() >= len)
    }
}

/// Feeds TS packets in and emits completed elementary-stream access units.
pub struct TsDemux {
    queue: SharedQueue,
    reader: TsPacketReader<SharedQueue>,
    es_pids: HashMap<Pid, EsKind>,
    partial: HashMap<Pid, Partial>,
    /// SCTE-35 PIDs and the section being reassembled on each (`None`
    /// until a section start is seen). These packets never reach `mpeg2ts`,
    /// which would try to parse them as PES.
    sections: HashMap<u16, Option<Vec<u8>>>,
}

/// Longest section a `section_length` can declare (12 bits) plus header.
const MAX_SECTION: usize = 3 + 0x0FFF;
const SCTE35_TABLE_ID: u8 = 0xFC;

impl Default for TsDemux {
    fn default() -> Self {
        Self::new()
    }
}

impl TsDemux {
    pub fn new() -> Self {
        let queue = SharedQueue::new();
        let reader = TsPacketReader::new(queue.clone());
        Self { queue, reader, es_pids: HashMap::new(), partial: HashMap::new(), sections: HashMap::new() }
    }

    /// Buffers newly received bytes: a live-mode SRT message is some whole
    /// number of 188-byte TS packets.
    pub fn feed(&mut self, data: &[u8]) {
        self.queue.push(data);
    }

    /// Parses every complete TS packet currently buffered, appending any
    /// elementary-stream access units it completed to `out`. Never panics
    /// on malformed input: a packet `mpeg2ts` can't parse is logged and
    /// skipped (see `NOTES.md` for the resync caveat this implies).
    pub fn drain(&mut self, out: &mut Vec<EsUnit>) {
        while self.queue.len() >= TS_PACKET_SIZE {
            if let Some(pid) = self.queue.front_pid()
                && self.sections.contains_key(&pid)
            {
                let packet = self.queue.pop_packet();
                self.handle_section_packet(pid, &packet, out);
                continue;
            }
            match self.reader.read_ts_packet() {
                Ok(Some(packet)) => {
                    let pid = packet.header.pid;
                    match packet.payload {
                        Some(TsPayload::Pmt(pmt)) => self.handle_pmt(pmt),
                        Some(TsPayload::PesStart(pes)) => self.handle_pes_start(pid, pes, out),
                        Some(TsPayload::PesContinuation(data)) => self.handle_pes_continuation(pid, &data, out),
                        _ => {}
                    }
                }
                Ok(None) => break,
                Err(err) => {
                    tracing::debug!(%err, "malformed TS packet dropped");
                }
            }
        }
    }

    /// End of input (a file, not a live connection): hands out every PES
    /// still in flight. A video PES has no length, so without this the last
    /// access unit of each PID would never be emitted.
    pub fn flush(&mut self, out: &mut Vec<EsUnit>) {
        let mut pids: Vec<Pid> = self.partial.keys().copied().collect();
        pids.sort();
        for pid in pids {
            if let Some(partial) = self.partial.remove(&pid) {
                out.push(partial.finish());
            }
        }
    }

    fn handle_pmt(&mut self, pmt: Pmt) {
        for es in pmt.es_info {
            match es_kind(es.stream_type) {
                Some(EsKind::Scte35) => {
                    self.sections.entry(es.elementary_pid.as_u16()).or_insert(None);
                }
                Some(kind) => {
                    self.es_pids.insert(es.elementary_pid, kind);
                }
                None => {}
            }
        }
    }

    /// Reassembles PSI sections on an SCTE-35 PID (ISO/IEC 13818-1
    /// 2.4.4: a `pointer_field` follows the header when
    /// `payload_unit_start_indicator` is set) and emits each complete
    /// `splice_info_section`. The CRC is checked later, by the parser.
    fn handle_section_packet(&mut self, pid: u16, pkt: &[u8; TS_PACKET_SIZE], out: &mut Vec<EsUnit>) {
        let pusi = pkt[1] & 0x40 != 0;
        let afc = (pkt[3] >> 4) & 0x3;
        if afc & 0x1 == 0 {
            return;
        }
        let mut start = 4;
        if afc & 0x2 != 0 {
            start += 1 + usize::from(pkt[4]);
        }
        if start >= TS_PACKET_SIZE {
            return;
        }
        let payload = &pkt[start..];
        let slot = self.sections.entry(pid).or_insert(None);
        if !pusi {
            if let Some(buf) = slot.as_mut() {
                buf.extend_from_slice(payload);
            }
            Self::complete_sections(slot, out);
            return;
        }
        let pointer = usize::from(payload[0]);
        let rest = &payload[1..];
        let split = pointer.min(rest.len());
        if let Some(buf) = slot.as_mut() {
            buf.extend_from_slice(&rest[..split]);
            Self::complete_sections(slot, out);
        }
        *slot = Some(rest[split..].to_vec());
        Self::complete_sections(slot, out);
    }

    /// Emits every whole section at the front of `slot`; what is left is
    /// the start of the next one, or nothing once stuffing (0xFF) begins.
    fn complete_sections(slot: &mut Option<Vec<u8>>, out: &mut Vec<EsUnit>) {
        while let Some(buf) = slot.as_mut() {
            if buf.first().is_none_or(|&t| t == 0xFF) {
                *slot = None;
                return;
            }
            if buf.len() < 3 {
                return;
            }
            let total = 3 + ((usize::from(buf[1] & 0x0F) << 8) | usize::from(buf[2]));
            if buf.len() < total {
                if buf.len() > MAX_SECTION {
                    *slot = None;
                }
                return;
            }
            let rest = buf.split_off(total);
            let section = std::mem::replace(buf, rest);
            if section[0] == SCTE35_TABLE_ID {
                out.push(EsUnit { kind: EsKind::Scte35, pts: None, dts: None, data: section });
            }
        }
    }

    fn handle_pes_start(&mut self, pid: Pid, pes: Pes, out: &mut Vec<EsUnit>) {
        let Some(&kind) = self.es_pids.get(&pid) else { return };
        // A PES already in flight for this PID (the `expected_len: None`
        // case, the norm for video) ends here: a new start is the only
        // signal that the previous one is complete.
        if let Some(prev) = self.partial.remove(&pid) {
            out.push(prev.finish());
        }
        let expected_len = (pes.pes_packet_len != 0)
            .then(|| (pes.pes_packet_len as usize).saturating_sub(optional_header_len(&pes.header) as usize));
        let partial = Partial {
            kind,
            pts: pes.header.pts.map(|t| t.as_u64()),
            dts: pes.header.dts.map(|t| t.as_u64()),
            data: pes.data.to_vec(),
            expected_len,
        };
        if partial.is_complete() {
            out.push(partial.finish());
        } else {
            self.partial.insert(pid, partial);
        }
    }

    fn handle_pes_continuation(&mut self, pid: Pid, data: &[u8], out: &mut Vec<EsUnit>) {
        let Some(partial) = self.partial.get_mut(&pid) else { return };
        partial.data.extend_from_slice(data);
        if partial.is_complete() {
            if let Some(partial) = self.partial.remove(&pid) {
                out.push(partial.finish());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PID: u16 = 0x1F0;

    fn packet(pusi: bool, cc: u8, payload: &[u8]) -> [u8; TS_PACKET_SIZE] {
        let mut p = [0xFF; TS_PACKET_SIZE];
        p[0] = 0x47;
        p[1] = (if pusi { 0x40 } else { 0 }) | (PID >> 8) as u8;
        p[2] = PID as u8;
        p[3] = 0x10 | (cc & 0x0F);
        p[4..4 + payload.len()].copy_from_slice(payload);
        p
    }

    fn fake_section(len: usize, fill: u8) -> Vec<u8> {
        let body = len - 3;
        let mut s = vec![fill; len];
        s[0] = 0xFC;
        s[1] = 0x30 | (body >> 8) as u8;
        s[2] = body as u8;
        s
    }

    fn demux() -> TsDemux {
        let mut d = TsDemux::new();
        d.sections.insert(PID, None);
        d
    }

    #[test]
    fn a_section_spanning_three_packets_is_reassembled() {
        let section = fake_section(400, 0xAB);
        let mut d = demux();
        let mut first = vec![0u8];
        first.extend_from_slice(&section[..183]);
        d.feed(&packet(true, 0, &first));
        d.feed(&packet(false, 1, &section[183..367]));
        d.feed(&packet(false, 2, &section[367..]));
        let mut out = Vec::new();
        d.drain(&mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, EsKind::Scte35);
        assert_eq!(out[0].data, section);
    }

    #[test]
    fn two_sections_in_one_packet_and_a_pointer_field_tail() {
        let a = fake_section(20, 1);
        let b = fake_section(30, 2);
        let c = fake_section(200, 3);
        let mut d = demux();
        // Packet 1: a, b, then the start of c.
        let mut p1 = vec![0u8];
        p1.extend_from_slice(&a);
        p1.extend_from_slice(&b);
        let c_head = 183 - a.len() - b.len();
        p1.extend_from_slice(&c[..c_head]);
        d.feed(&packet(true, 0, &p1));
        // Packet 2 starts a new section: its pointer_field skips c's tail.
        let tail = &c[c_head..];
        let mut p2 = vec![tail.len() as u8];
        p2.extend_from_slice(tail);
        p2.extend_from_slice(&a);
        d.feed(&packet(true, 1, &p2));
        let mut out = Vec::new();
        d.drain(&mut out);
        let got: Vec<&Vec<u8>> = out.iter().map(|u| &u.data).collect();
        assert_eq!(got, vec![&a, &b, &c, &a]);
    }

    #[test]
    fn other_tables_and_garbage_are_ignored() {
        let mut d = demux();
        let mut other = fake_section(20, 0);
        other[0] = 0x02;
        let mut p = vec![0u8];
        p.extend_from_slice(&other);
        d.feed(&packet(true, 0, &p));
        // A continuation with no section started is dropped.
        d.feed(&packet(false, 1, &[0xFC, 0x30, 0x05, 1, 2, 3, 4, 5]));
        let mut out = Vec::new();
        d.drain(&mut out);
        assert!(out.is_empty());
    }
}
