//! A minimal live Matroska writer for ffmpeg's stdin. The `Ffmpeg` engine
//! uses it only when the source's audio is Opus: `caudal_ts::mux::TsMux`
//! has no Opus mapping and drops it, but WHIP sources are H.264 + Opus and
//! must come out with AAC. `caudal-omt` uses it for raw video
//! (`V_UNCOMPRESSED` + a FourCC `ColourSpace`) and float PCM
//! (`A_PCM/FLOAT/IEEE`); see `caudal-omt/NOTES.md` for ffmpeg's support.
//!
//! Layout: EBML header, a Segment of unknown size with Info (timecode scale:
//! 1 ms from [`MkvWriter::new`], any from [`MkvWriter::with_tracks`]) and
//! Tracks, then unknown-size Clusters of SimpleBlocks (block timecode =
//! PTS). Video blocks carry the AVCC/HVCC payload as is, with the
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

/// One track of a [`MkvWriter`].
#[derive(Debug, Clone, Copy)]
pub enum MkvTrack<'a> {
    /// H.264 / H.265 / Opus described by a Caudal track (others are skipped).
    Coded(&'a TrackInfo),
    /// Uncompressed video, one whole picture per block, tightly packed in
    /// the layout `fourcc` names (e.g. `UYVY`, `NV12`, `I420`).
    RawVideo { id: TrackId, width: u32, height: u32, fourcc: [u8; 4] },
    /// Interleaved 32-bit float little-endian PCM.
    PcmF32 { id: TrackId, sample_rate: u32, channels: u8 },
}

pub struct MkvWriter {
    /// Source track id → Matroska track number.
    tracks: Vec<(TrackId, u64)>,
    /// Current cluster's timecode, in ticks of `scale_ns`.
    cluster: Option<i64>,
    /// TimecodeScale, nanoseconds per tick.
    scale_ns: i64,
}

impl MkvWriter {
    /// Returns the writer and the header bytes (EBML header, Segment start,
    /// Info, Tracks), with a 1 ms timecode scale. Tracks it can't describe
    /// are left out.
    pub fn new(tracks: &[&TrackInfo]) -> (Self, Vec<u8>) {
        let tracks: Vec<MkvTrack> = tracks.iter().map(|t| MkvTrack::Coded(t)).collect();
        Self::with_tracks(&tracks, 1_000_000)
    }

    /// Like [`MkvWriter::new`] with any track kinds and TimecodeScale
    /// `scale_ns` nanoseconds per tick (1 000 = microseconds). Block
    /// timecodes are 16-bit relative to their cluster, so a small scale
    /// just means more clusters.
    pub fn with_tracks(tracks: &[MkvTrack], scale_ns: u32) -> (Self, Vec<u8>) {
        let scale_ns = scale_ns.max(1);
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
                uint(&[0x2A, 0xD7, 0xB1], u64::from(scale_ns)),
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
            let id = match *t {
                MkvTrack::Coded(t) => {
                    match t.codec {
                        Codec::H264 | Codec::H265 => {
                            let id: &[u8] =
                                if t.codec == Codec::H264 { b"V_MPEG4/ISO/AVC" } else { b"V_MPEGH/ISO/HEVC" };
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
                            e.push(element(
                                &[0xE1],
                                &[float(&[0xB5], f64::from(rate)), uint(&[0x9F], u64::from(ch))].concat(),
                            ));
                        }
                        _ => continue,
                    }
                    t.id
                }
                MkvTrack::RawVideo { id, width, height, fourcc } => {
                    e.push(uint(&[0x83], 1));
                    e.push(element(&[0x86], b"V_UNCOMPRESSED"));
                    e.push(element(
                        &[0xE0],
                        &[
                            uint(&[0xB0], u64::from(width)),
                            uint(&[0xBA], u64::from(height)),
                            // ColourSpace: the FourCC ffmpeg maps to a pixel format.
                            element(&[0x2E, 0xB5, 0x24], &fourcc),
                        ]
                        .concat(),
                    ));
                    id
                }
                MkvTrack::PcmF32 { id, sample_rate, channels } => {
                    e.push(uint(&[0x83], 2));
                    e.push(element(&[0x86], b"A_PCM/FLOAT/IEEE"));
                    e.push(element(
                        &[0xE1],
                        &[
                            float(&[0xB5], f64::from(sample_rate)),
                            uint(&[0x9F], u64::from(channels)),
                            uint(&[0x62, 0x64], 32),
                        ]
                        .concat(),
                    ));
                    id
                }
            };
            entries.push(element(&[0xAE], &e.concat()));
            map.push((id, number));
        }
        out.extend_from_slice(&element(&[0x16, 0x54, 0xAE, 0x6B], &entries.concat()));
        (Self { tracks: map, cluster: None, scale_ns: i64::from(scale_ns) }, out)
    }

    /// One SimpleBlock (and a new Cluster when the relative timecode would
    /// leave ±30 s, or on a video keyframe more than 1 s into the cluster).
    pub fn write(&mut self, info: &TrackInfo, frame: &Frame, out: &mut Vec<u8>) {
        let video = info.video.is_some();
        let at_us = info.to_micros(frame.pts);
        if self.block_header(frame.track, at_us, video && frame.keyframe, frame.keyframe, frame.data.len(), out) {
            out.extend_from_slice(&frame.data);
        }
    }

    /// Writes everything of one SimpleBlock but its payload (a Cluster
    /// first if needed): the caller appends exactly `len` payload bytes
    /// next, so large raw frames need not be copied into `out`. `cut`
    /// allows a new cluster once 1 s into the current one (video keyframes).
    /// Returns false (and writes nothing) for a track the writer doesn't
    /// have.
    pub fn block_header(
        &mut self,
        track: TrackId,
        at_us: i64,
        cut: bool,
        keyframe: bool,
        len: usize,
        out: &mut Vec<u8>,
    ) -> bool {
        let Some(&(_, number)) = self.tracks.iter().find(|(id, _)| *id == track) else { return false };
        let t = (i128::from(at_us) * 1000).div_euclid(i128::from(self.scale_ns)) as i64;
        // 30 000 ticks (30 s at 1 ms) keeps the relative timecode in i16.
        let second = 1_000_000_000 / self.scale_ns;
        let new_cluster = match self.cluster {
            None => true,
            Some(c) => (t - c).abs() > 30_000 || (cut && t - c >= second),
        };
        if new_cluster {
            let c = t.max(0);
            out.extend_from_slice(&[0x1F, 0x43, 0xB6, 0x75]);
            out.extend_from_slice(&UNKNOWN_SIZE);
            out.extend_from_slice(&uint(&[0xE7], c as u64));
            self.cluster = Some(c);
        }
        let rel = (t - self.cluster.unwrap_or(0)).clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i16;
        out.push(0xA3);
        out.extend_from_slice(&size_vint(len + 4));
        out.push(0x80 | number as u8);
        out.extend_from_slice(&rel.to_be_bytes());
        out.push(if keyframe { 0x80 } else { 0 });
        true
    }
}
