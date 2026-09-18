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
}

fn es_kind(stream_type: StreamType) -> Option<EsKind> {
    match stream_type {
        StreamType::H264 => Some(EsKind::H264),
        StreamType::H265 => Some(EsKind::H265),
        StreamType::AdtsAac => Some(EsKind::Aac),
        _ => None,
    }
}

/// One reassembled PES packet: raw elementary-stream payload plus its
/// wire-clock (33-bit, 90 kHz) timestamps, not yet unwrapped or rebased.
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
}

impl Default for TsDemux {
    fn default() -> Self {
        Self::new()
    }
}

impl TsDemux {
    pub fn new() -> Self {
        let queue = SharedQueue::new();
        let reader = TsPacketReader::new(queue.clone());
        Self { queue, reader, es_pids: HashMap::new(), partial: HashMap::new() }
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
            if let Some(kind) = es_kind(es.stream_type) {
                self.es_pids.insert(es.elementary_pid, kind);
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
