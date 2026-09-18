//! A minimal live Matroska writer, used as ffmpeg's input only when the
//! source's audio is Opus: `caudal_ts::mux::TsMux` has no Opus mapping and
//! drops it, but WHIP sources are H.264 + Opus and must come out with AAC.
//!
//! Layout: EBML header, a Segment of unknown size with Info (1 ms timecode
//! scale) and Tracks, then unknown-size Clusters of SimpleBlocks (block
//! timecode = PTS). Video blocks carry the AVCC/HVCC payload as is, with the
//! `avcC`/`hvcC` record as CodecPrivate; Opus uses the `OpusHead` as
//! CodecPrivate.

use caudal_core::{Codec, Frame, TrackId, TrackInfo};

const UNKNOWN_SIZE: [u8; 8] = [0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];

fn size_vint(n: usize) -> Vec<u8> {
    // 8-byte sizes everywhere: simple and always valid.
    let mut v = (n as u64 | (1u64 << 56)).to_be_bytes().to_vec();
    v[0] = 0x01;
    v
}

fn element(id: &[u8], body: &[u8]) -> Vec<u8> {
    let mut out = id.to_vec();
    out.extend_from_slice(&size_vint(body.len()));
    out.extend_from_slice(body);
    out
}

fn uint(id: &[u8], v: u64) -> Vec<u8> {
    let bytes = v.to_be_bytes();
    let skip = bytes.iter().take(7).take_while(|&&b| b == 0).count();
    element(id, &bytes[skip..])
}

fn float(id: &[u8], v: f64) -> Vec<u8> {
    element(id, &v.to_be_bytes())
}

pub(crate) struct MkvWriter {
    /// Source track id → Matroska track number.
    tracks: Vec<(TrackId, u64)>,
    cluster_ms: Option<i64>,
}

impl MkvWriter {
    /// Returns the writer and the header bytes (EBML header, Segment start,
    /// Info, Tracks). Tracks it can't describe are left out.
    pub fn new(tracks: &[&TrackInfo]) -> (Self, Vec<u8>) {
        let mut out = element(
            &[0x1A, 0x45, 0xDF, 0xA3],
            &[
                uint(&[0x42, 0x86], 1),
                uint(&[0x42, 0xF7], 1),
                uint(&[0x42, 0xF2], 4),
                uint(&[0x42, 0xF3], 8),
                element(&[0x42, 0x82], b"matroska"),
                uint(&[0x42, 0x87], 4),
                uint(&[0x42, 0x85], 2),
            ]
            .concat(),
        );
        out.extend_from_slice(&[0x18, 0x53, 0x80, 0x67]);
        out.extend_from_slice(&UNKNOWN_SIZE);
        out.extend_from_slice(&element(
            &[0x15, 0x49, 0xA9, 0x66],
            &[
                uint(&[0x2A, 0xD7, 0xB1], 1_000_000),
                element(&[0x4D, 0x80], b"caudal"),
                element(&[0x57, 0x41], b"caudal"),
            ]
            .concat(),
        ));
        let mut entries = Vec::new();
        let mut map = Vec::new();
        for t in tracks {
            let number = map.len() as u64 + 1;
            let mut e = vec![uint(&[0xD7], number), uint(&[0x73, 0xC5], number), uint(&[0x9C], 0)];
            match t.codec {
                Codec::H264 | Codec::H265 => {
                    let id: &[u8] = if t.codec == Codec::H264 { b"V_MPEG4/ISO/AVC" } else { b"V_MPEGH/ISO/HEVC" };
                    e.push(uint(&[0x83], 1));
                    e.push(element(&[0x86], id));
                    e.push(element(&[0x63, 0xA2], &t.init));
                    if let Some(v) = t.video {
                        e.push(element(
                            &[0xE0],
                            &[uint(&[0xB0], u64::from(v.width)), uint(&[0xBA], u64::from(v.height))].concat(),
                        ));
                    }
                }
                Codec::Opus => {
                    let (rate, ch) = t.audio.map_or((48_000, 2), |a| (a.sample_rate, a.channels));
                    e.push(uint(&[0x83], 2));
                    e.push(element(&[0x86], b"A_OPUS"));
                    e.push(element(&[0x63, 0xA2], &t.init));
                    e.push(element(&[0xE1], &[float(&[0xB5], f64::from(rate)), uint(&[0x9F], u64::from(ch))].concat()));
                }
                _ => continue,
            }
            entries.push(element(&[0xAE], &e.concat()));
            map.push((t.id, number));
        }
        out.extend_from_slice(&element(&[0x16, 0x54, 0xAE, 0x6B], &entries.concat()));
        (Self { tracks: map, cluster_ms: None }, out)
    }

    /// One SimpleBlock (and a new Cluster when the relative timecode would
    /// leave ±30 s, or on a video keyframe more than 1 s into the cluster).
    pub fn write(&mut self, info: &TrackInfo, frame: &Frame, out: &mut Vec<u8>) {
        let Some(&(_, number)) = self.tracks.iter().find(|(id, _)| *id == frame.track) else { return };
        let ms = info.to_micros(frame.pts).div_euclid(1000);
        let video = info.video.is_some();
        let new_cluster = match self.cluster_ms {
            None => true,
            Some(c) => (ms - c).abs() > 30_000 || (video && frame.keyframe && ms - c >= 1000),
        };
        if new_cluster {
            let c = ms.max(0);
            out.extend_from_slice(&[0x1F, 0x43, 0xB6, 0x75]);
            out.extend_from_slice(&UNKNOWN_SIZE);
            out.extend_from_slice(&uint(&[0xE7], c as u64));
            self.cluster_ms = Some(c);
        }
        let rel = (ms - self.cluster_ms.unwrap_or(0)).clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i16;
        let mut body = Vec::with_capacity(frame.data.len() + 4);
        body.push(0x80 | number as u8);
        body.extend_from_slice(&rel.to_be_bytes());
        body.push(if frame.keyframe { 0x80 } else { 0 });
        body.extend_from_slice(&frame.data);
        out.extend_from_slice(&element(&[0xA3], &body));
    }
}
