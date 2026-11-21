//! Reading live fragmented MP4 / CMAF, the other direction of
//! [`crate::fmp4`]: an init segment (`ftyp` + `moov`, an HLS `EXT-X-MAP`)
//! gives the tracks, then every media segment (`moof` + `mdat` pairs, any
//! number of them, `styp` / `sidx` / `prft` / `emsg` skipped) gives frames.
//!
//! - Tracks: H.264 (`avc1`), H.265 (`hvc1`/`hev1`), AV1, AAC and
//!   Opus; others (text, timed metadata) are left out. Track ids are
//!   numbered `0..` in `moov` order, and each track keeps its `mdhd`
//!   timescale, so timestamps are exact (no rescaling).
//! - Samples: `tfdt` base decode time (or where the track's previous
//!   fragment ended), `trun` durations, sizes, composition offsets and
//!   flags, falling back to the `tfhd` and then the `trex` defaults; data
//!   offsets per ISO/IEC 14496-12 8.8.7 (`base_data_offset`,
//!   `default-base-is-moof`, or the end of the previous track run).
//! - NAL units with 1- or 2-byte length prefixes are rewritten to the
//!   4-byte prefixes Caudal frames carry (and the codec record with them).
//! - Encrypted (`senc`, `encv`/`enca`) content is not decrypted.

use bytes::{Bytes, BytesMut};
use caudal_core::{AudioParams, Codec, Frame, TrackId, TrackInfo, VideoParams};
use mp4_atom::{Atom, Decode, Moof, Moov};

/// `sample_is_non_sync_sample` in ISO BMFF sample flags.
const NON_SYNC: u32 = 0x0001_0000;

struct Track {
    /// The MP4 `track_id` (`tkhd`, `tfhd`).
    mp4_id: u32,
    info: TrackInfo,
    video: bool,
    /// `trex` defaults: duration, size, flags.
    defaults: (u32, u32, u32),
    /// NAL length-prefix size when it is not 4 (video only).
    nal_len: Option<u8>,
    /// Decode time where the last fragment of this track ended.
    next_dts: Option<i64>,
}

/// One fMP4 source: its init segment, then its media segments in order.
pub struct Fmp4Demux {
    tracks: Vec<Track>,
}

/// One top-level box: its type and its byte range (header included).
fn boxes(data: &[u8]) -> Result<Vec<([u8; 4], usize, usize)>, String> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < data.len() {
        let rest = &data[at..];
        if rest.len() < 8 {
            return Err(format!("{} stray bytes after the last box", rest.len()));
        }
        let size32 = u32::from_be_bytes(rest[0..4].try_into().unwrap());
        let kind: [u8; 4] = rest[4..8].try_into().unwrap();
        let size = match size32 {
            0 => rest.len() as u64,
            1 => {
                if rest.len() < 16 {
                    return Err("truncated 64-bit box size".into());
                }
                u64::from_be_bytes(rest[8..16].try_into().unwrap())
            }
            n => u64::from(n),
        };
        let min = if size32 == 1 { 16 } else { 8 };
        if size < min || size > rest.len() as u64 {
            return Err(format!("box {:?} of {size} bytes does not fit in {}", fourcc(&kind), rest.len()));
        }
        out.push((kind, at, at + size as usize));
        at += size as usize;
    }
    Ok(out)
}

fn fourcc(k: &[u8; 4]) -> String {
    String::from_utf8_lossy(k).into_owned()
}

/// RFC 7845 `OpusHead` (little-endian) from an ISO BMFF `dOps` (big-endian).
fn opus_head(d: &mp4_atom::Dops) -> Vec<u8> {
    let mut h = b"OpusHead".to_vec();
    h.push(1);
    h.push(d.output_channel_count);
    h.extend_from_slice(&d.pre_skip.to_le_bytes());
    h.extend_from_slice(&d.input_sample_rate.to_le_bytes());
    h.extend_from_slice(&d.output_gain.to_le_bytes());
    h.push(0);
    h
}

fn encode_body<T: Atom>(atom: &T) -> Result<Vec<u8>, String> {
    let mut v = Vec::new();
    atom.encode_body(&mut v).map_err(|e| format!("codec record: {e}"))?;
    Ok(v)
}

fn visual(v: &mp4_atom::Visual) -> Option<VideoParams> {
    Some(VideoParams { width: v.width.into(), height: v.height.into(), fps: None })
}

/// Rewrites `n`-byte NAL length prefixes into 4-byte ones.
fn widen_nal_lengths(data: &[u8], n: u8) -> Result<Bytes, String> {
    let n = usize::from(n);
    let mut out = BytesMut::with_capacity(data.len() + data.len() / 8);
    let mut at = 0;
    while at < data.len() {
        let head = data.get(at..at + n).ok_or("truncated NAL length")?;
        let len = head.iter().fold(0usize, |a, &b| (a << 8) | usize::from(b));
        let nal = data.get(at + n..at + n + len).ok_or("NAL unit past the sample end")?;
        out.extend_from_slice(&(len as u32).to_be_bytes());
        out.extend_from_slice(nal);
        at += n + len;
    }
    Ok(out.freeze())
}

impl Fmp4Demux {
    /// Parses an init segment. Fails when it has no `moov` or no track this
    /// demuxer can read.
    pub fn new(init: &[u8]) -> Result<Self, String> {
        let mut moov = None;
        for (kind, from, to) in boxes(init)? {
            if &kind == b"moov" {
                let m = Moov::decode(&mut &init[from..to]).map_err(|e| format!("moov: {e}"))?;
                moov = Some(m);
            }
        }
        let moov = moov.ok_or("the init segment has no moov")?;
        let trex = moov.mvex.as_ref().map(|m| m.trex.clone()).unwrap_or_default();
        let mut tracks = Vec::new();
        for trak in &moov.trak {
            let ts = trak.mdia.mdhd.timescale;
            if ts == 0 {
                continue;
            }
            let mut nal_len = None;
            let (codec, init, video, audio) = match trak.mdia.minf.stbl.stsd.codecs.first() {
                Some(mp4_atom::Codec::Avc1(a)) => {
                    let mut avcc = a.avcc.clone();
                    if avcc.length_size != 4 {
                        nal_len = Some(avcc.length_size);
                        avcc.length_size = 4;
                    }
                    (Codec::H264, encode_body(&avcc)?, visual(&a.visual), None)
                }
                Some(mp4_atom::Codec::Hvc1(h)) => {
                    let mut hvcc = h.hvcc.clone();
                    if hvcc.length_size_minus_one != 3 {
                        nal_len = Some(hvcc.length_size_minus_one + 1);
                        hvcc.length_size_minus_one = 3;
                    }
                    (Codec::H265, encode_body(&hvcc)?, visual(&h.visual), None)
                }
                Some(mp4_atom::Codec::Hev1(h)) => {
                    let mut hvcc = h.hvcc.clone();
                    if hvcc.length_size_minus_one != 3 {
                        nal_len = Some(hvcc.length_size_minus_one + 1);
                        hvcc.length_size_minus_one = 3;
                    }
                    (Codec::H265, encode_body(&hvcc)?, visual(&h.visual), None)
                }
                Some(mp4_atom::Codec::Av01(a)) => (Codec::Av1, encode_body(&a.av1c)?, visual(&a.visual), None),
                Some(mp4_atom::Codec::Mp4a(m)) => {
                    let Some(asc) = m.esds.es_desc.dec_config.dec_specific.as_ref() else { continue };
                    let a = AudioParams { sample_rate: ts, channels: m.audio.channel_count as u8 };
                    (Codec::Aac, asc.raw.clone(), None, Some(a))
                }
                Some(mp4_atom::Codec::Opus(o)) => {
                    let a = AudioParams { sample_rate: ts, channels: o.dops.output_channel_count };
                    (Codec::Opus, opus_head(&o.dops), None, Some(a))
                }
                _ => continue,
            };
            let mp4_id = trak.tkhd.track_id;
            let defaults = trex
                .iter()
                .find(|t| t.track_id == mp4_id)
                .map_or((0, 0, 0), |t| (t.default_sample_duration, t.default_sample_size, t.default_sample_flags));
            let lang = Some(trak.mdia.mdhd.language.clone()).filter(|l| l.len() == 3 && l != "und");
            let is_video = video.is_some();
            let info = TrackInfo {
                id: TrackId(tracks.len() as u32),
                codec,
                timescale: ts,
                init: Bytes::from(init),
                lang,
                video,
                audio,
            };
            tracks.push(Track { mp4_id, info, video: is_video, defaults, nal_len, next_dts: None });
        }
        if tracks.is_empty() {
            return Err("no H.264, H.265, AV1, AAC or Opus track in the init segment".into());
        }
        Ok(Self { tracks })
    }

    /// The tracks to announce, ids `0..`.
    pub fn tracks(&self) -> Vec<TrackInfo> {
        self.tracks.iter().map(|t| t.info.clone()).collect()
    }

    /// Every frame of one media segment, ordered by decode time across
    /// tracks. Samples of tracks the init did not declare are skipped; a
    /// malformed fragment fails the whole segment.
    pub fn segment(&mut self, data: &Bytes) -> Result<Vec<Frame>, String> {
        let mut frames: Vec<(i128, Frame)> = Vec::new();
        for (kind, from, to) in boxes(data)? {
            if &kind != b"moof" {
                continue;
            }
            let moof = Moof::decode(&mut &data[from..to]).map_err(|e| format!("moof: {e}"))?;
            // Where the data of a run without its own offset starts: after
            // the previous run (ISO/IEC 14496-12 8.8.7.1).
            let mut prev_end = from as u64;
            for traf in &moof.traf {
                let tfhd = &traf.tfhd;
                let base = match tfhd.base_data_offset {
                    Some(b) => b,
                    None if tfhd.default_base_is_moof => from as u64,
                    None => prev_end,
                };
                let Some(track) = self.tracks.iter_mut().find(|t| t.mp4_id == tfhd.track_id) else {
                    continue;
                };
                let duration = tfhd.default_sample_duration.unwrap_or(track.defaults.0);
                let size = tfhd.default_sample_size.unwrap_or(track.defaults.1);
                let flags = tfhd.default_sample_flags.unwrap_or(track.defaults.2);
                let mut dts = match (&traf.tfdt, track.next_dts) {
                    (Some(t), _) => i64::try_from(t.base_media_decode_time).map_err(|_| "tfdt out of range")?,
                    (None, Some(d)) => d,
                    (None, None) => 0,
                };
                let mut pos = base;
                for (i, trun) in traf.trun.iter().enumerate() {
                    if let Some(off) = trun.data_offset {
                        pos = base.checked_add_signed(i64::from(off)).ok_or("negative data offset")?;
                    } else if i == 0 {
                        pos = base;
                    }
                    for e in &trun.entries {
                        let len = e.size.unwrap_or(size) as u64;
                        let start = usize::try_from(pos).map_err(|_| "data offset out of range")?;
                        let end = usize::try_from(pos + len).map_err(|_| "data offset out of range")?;
                        if end > data.len() {
                            return Err(format!(
                                "a sample at {start}..{end} lies past the segment end ({})",
                                data.len()
                            ));
                        }
                        let raw = data.slice(start..end);
                        let payload = match track.nal_len {
                            Some(n) => widen_nal_lengths(&raw, n)?,
                            None => raw,
                        };
                        let f = e.flags.unwrap_or(flags);
                        let keyframe = !track.video || f & NON_SYNC == 0;
                        let cts = i64::from(e.cts.unwrap_or(0));
                        let frame = Frame { track: track.info.id, dts, pts: dts + cts, keyframe, data: payload };
                        let at = i128::from(dts) * 1_000_000 / i128::from(track.info.timescale);
                        frames.push((at, frame));
                        dts += i64::from(e.duration.unwrap_or(duration));
                        pos += len;
                    }
                }
                prev_end = pos;
                track.next_dts = Some(dts);
            }
        }
        frames.sort_by_key(|(at, f)| (*at, f.track.0));
        Ok(frames.into_iter().map(|(_, f)| f).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fmp4::{Mp4Track, Run, Sample, fragment, init_segment};
    use mp4_atom::Encode;

    fn avcc() -> Bytes {
        // avcC: version 1, High@3.1, 4-byte lengths, one SPS, one PPS.
        let sps = [0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40, 0x50, 0x05, 0xbb, 0x01, 0x10];
        let pps = [0x68, 0xeb, 0xe3, 0xcb, 0x22, 0xc0];
        let mut v = vec![1, 0x64, 0x00, 0x1f, 0xff, 0xe1];
        v.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        v.extend_from_slice(&sps);
        v.push(1);
        v.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        v.extend_from_slice(&pps);
        Bytes::from(v)
    }

    fn tracks() -> Vec<Mp4Track> {
        let video = TrackInfo {
            id: TrackId(0),
            codec: Codec::H264,
            timescale: 90_000,
            init: avcc(),
            lang: None,
            video: Some(VideoParams { width: 640, height: 360, fps: None }),
            audio: None,
        };
        let audio = TrackInfo {
            id: TrackId(1),
            codec: Codec::Aac,
            timescale: 48_000,
            init: Bytes::from_static(&[0x11, 0x90]),
            lang: Some("spa".into()),
            video: None,
            audio: Some(AudioParams { sample_rate: 48_000, channels: 2 }),
        };
        vec![Mp4Track { track_id: 1, info: video }, Mp4Track { track_id: 2, info: audio }]
    }

    fn sample(dts: i64, cts: i32, dur: u32, key: bool, tag: u8) -> Sample {
        Sample { dts, cts, dur, key, data: Bytes::from(vec![0, 0, 0, 2, 0x65, tag]) }
    }

    #[test]
    fn caudal_init_and_fragments_round_trip() {
        let init = init_segment(&tracks()).unwrap();
        let mut d = Fmp4Demux::new(&init).unwrap();
        let t = d.tracks();
        assert_eq!(t.len(), 2);
        assert_eq!((t[0].codec, t[0].timescale, t[0].init.clone()), (Codec::H264, 90_000, avcc()));
        assert_eq!(t[0].video.map(|v| (v.width, v.height)), Some((640, 360)));
        assert_eq!((t[1].codec, t[1].timescale, &t[1].init[..]), (Codec::Aac, 48_000, &[0x11, 0x90][..]));
        assert_eq!(t[1].audio, Some(AudioParams { sample_rate: 48_000, channels: 2 }));
        assert_eq!(t[1].lang.as_deref(), Some("spa"));

        // Two fragments in one segment, as Caudal's full segments are
        // (their parts back to back).
        let v1 = [sample(9000, 6000, 3000, true, 1), sample(12_000, 0, 3000, false, 2)];
        let a1 = [sample(4800, 0, 1024, true, 3)];
        let v2 = [sample(15_000, 3000, 3000, false, 4)];
        let a2 = [sample(5824, 0, 1024, true, 5), sample(6848, 0, 1024, true, 6)];
        let mut seg = BytesMut::new();
        seg.extend_from_slice(&fragment(1, &[Run { track_id: 1, samples: &v1 }, Run { track_id: 2, samples: &a1 }]));
        seg.extend_from_slice(&fragment(2, &[Run { track_id: 1, samples: &v2 }, Run { track_id: 2, samples: &a2 }]));
        let frames = d.segment(&seg.freeze()).unwrap();
        let got: Vec<(u32, i64, i64, bool, u8)> =
            frames.iter().map(|f| (f.track.0, f.dts, f.pts, f.keyframe, f.data[5])).collect();
        assert_eq!(
            got,
            [
                (0, 9000, 15_000, true, 1),
                (1, 4800, 4800, true, 3),
                (1, 5824, 5824, true, 5),
                (0, 12_000, 12_000, false, 2),
                (1, 6848, 6848, true, 6),
                (0, 15_000, 18_000, false, 4),
            ],
            "ordered by time, flags, offsets and payloads kept"
        );
    }

    #[test]
    fn defaults_and_missing_tfdt() {
        let init = init_segment(&tracks()[..1]).unwrap();
        let mut d = Fmp4Demux::new(&init).unwrap();
        // A moof whose trun has sizes only: durations and flags come from
        // the tfhd, and no tfdt the second time.
        let build = |tfdt: Option<u64>| {
            let payload = [0u8, 0, 0, 1, 0x41, 0, 0, 0, 1, 0x41];
            let mut moof = Moof {
                mfhd: mp4_atom::Mfhd { sequence_number: 1 },
                traf: vec![mp4_atom::Traf {
                    tfhd: mp4_atom::Tfhd {
                        track_id: 1,
                        default_sample_duration: Some(3000),
                        default_sample_flags: Some(NON_SYNC),
                        default_base_is_moof: true,
                        ..Default::default()
                    },
                    tfdt: tfdt.map(|t| mp4_atom::Tfdt { base_media_decode_time: t }),
                    trun: vec![mp4_atom::Trun {
                        data_offset: Some(0),
                        entries: vec![
                            mp4_atom::TrunEntry { size: Some(5), ..Default::default() },
                            mp4_atom::TrunEntry { size: Some(5), ..Default::default() },
                        ],
                    }],
                    ..Default::default()
                }],
            };
            let mut probe = Vec::new();
            moof.encode(&mut probe).unwrap();
            moof.traf[0].trun[0].data_offset = Some(probe.len() as i32 + 8);
            let mut out = Vec::new();
            moof.encode(&mut out).unwrap();
            out.extend_from_slice(&18u32.to_be_bytes());
            out.extend_from_slice(b"mdat");
            out.extend_from_slice(&payload);
            Bytes::from(out)
        };
        let f = d.segment(&build(Some(90_000))).unwrap();
        assert_eq!(f.iter().map(|f| (f.dts, f.keyframe)).collect::<Vec<_>>(), [(90_000, false), (93_000, false)]);
        let f = d.segment(&build(None)).unwrap();
        assert_eq!(f[0].dts, 96_000, "no tfdt: continues where the track stopped");
    }

    #[test]
    fn short_nal_lengths_are_widened() {
        assert_eq!(
            &widen_nal_lengths(&[0, 2, 0x65, 1, 0, 1, 0x06], 2).unwrap()[..],
            [0, 0, 0, 2, 0x65, 1, 0, 0, 0, 1, 0x06]
        );
        assert!(widen_nal_lengths(&[0, 5, 0x65], 2).is_err());
    }

    #[test]
    fn refusals() {
        assert!(Fmp4Demux::new(b"").err().unwrap().contains("moov"));
        assert!(Fmp4Demux::new(&[0, 0, 0, 9, b'f', b't', b'y', b'p']).is_err(), "box past the end");
        let mut d = Fmp4Demux::new(&init_segment(&tracks()).unwrap()).unwrap();
        assert!(d.segment(&Bytes::from_static(&[0, 0, 0, 8, b's', b't'])).is_err());
        let mut seg = fragment(1, &[Run { track_id: 1, samples: &[sample(0, 0, 1, true, 1)] }]).to_vec();
        seg.truncate(seg.len() - 2); // the mdat header now claims more than there is
        assert!(d.segment(&Bytes::from(seg)).is_err(), "truncated mdat");
        assert!(d.segment(&Bytes::new()).unwrap().is_empty());
    }
}
