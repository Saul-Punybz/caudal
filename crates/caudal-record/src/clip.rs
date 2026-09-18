//! Clips: a time range of a recording as one progressive MP4 (`ftyp` +
//! `moov` with full sample tables + `mdat`), built from the recording's own
//! `init.mp4` and segments.
//!
//! Nothing is re-encoded. The `moov` is the recording's init `moov` with the
//! fragment boxes (`mvex`) taken out and the sample tables filled in; the
//! `mdat` is a list of byte ranges copied straight out of the segment files,
//! one chunk per track run of every fragment, so the output is naturally
//! interleaved and the body can be streamed without holding it in memory.
//!
//! Timeline: milliseconds from the start of the recording, as a VOD player
//! sees it (the sum of the playlist's `#EXTINF` durations before a segment,
//! plus the position inside it). The start snaps back to the keyframe at or
//! before `from_ms`; `to_ms` is exclusive.

use std::path::{Path, PathBuf};

use mp4_atom::{
    Any, Co64, Ctts, CttsEntry, Decode, Edts, Elst, ElstEntry, Encode, Ftyp, Moov, Stco, Stsc, StscEntry, Stss, Stsz,
    StszSamples, Stts, SttsEntry, Trak,
};

use crate::meta::{parse_segment_name, playlist_segments};

/// Largest clip we serve.
pub(crate) const MAX_CLIP_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug)]
pub(crate) enum ClipError {
    NotFound,
    BadRange(&'static str),
    TooLarge,
    Io(std::io::Error),
    Corrupt(String),
}

impl From<std::io::Error> for ClipError {
    fn from(e: std::io::Error) -> Self {
        if e.kind() == std::io::ErrorKind::NotFound { ClipError::NotFound } else { ClipError::Io(e) }
    }
}

/// A byte range of a segment file, copied into the `mdat` as is.
#[derive(Debug, Clone)]
pub(crate) struct Piece {
    pub file: PathBuf,
    pub offset: u64,
    pub len: u64,
}

pub(crate) struct ClipPlan {
    /// `ftyp` + `moov` + the `mdat` header.
    pub header: Vec<u8>,
    pub pieces: Vec<Piece>,
    pub total: u64,
}

#[derive(Debug, Clone)]
struct SampleRef {
    /// Index into the output tracks.
    track: usize,
    dts: i64,
    dur: u32,
    cts: i32,
    key: bool,
    seg: usize,
    offset: u64,
    size: u32,
    /// Decode time on the recording's timeline, microseconds.
    t_us: i64,
}

struct TrackIn {
    track_id: u32,
    timescale: u32,
    trak: Trak,
}

fn micros(ticks: i64, ts: u32) -> i64 {
    (i128::from(ticks) * 1_000_000 / i128::from(ts.max(1))) as i64
}

fn corrupt(what: impl Into<String>) -> ClipError {
    ClipError::Corrupt(what.into())
}

/// Plans the clip `[from_ms, to_ms)` of the recording in `dir`.
pub(crate) async fn plan(dir: &Path, from_ms: i64, to_ms: i64) -> Result<ClipPlan, ClipError> {
    if from_ms < 0 || to_ms <= from_ms {
        return Err(ClipError::BadRange("need 0 <= from_ms < to_ms"));
    }
    let init = tokio::fs::read(dir.join("init.mp4")).await?;
    let playlist = String::from_utf8_lossy(&tokio::fs::read(dir.join("index.m3u8")).await?).into_owned();

    let mut moov = None;
    let mut buf = &init[..];
    while !buf.is_empty() {
        match Any::decode(&mut buf).map_err(|e| corrupt(format!("init.mp4: {e}")))? {
            Any::Moov(m) => moov = Some(m),
            _ => continue,
        }
    }
    let moov: Moov = moov.ok_or_else(|| corrupt("init.mp4 has no moov"))?;
    let mut tracks: Vec<TrackIn> = moov
        .trak
        .iter()
        .map(|t| TrackIn { track_id: t.tkhd.track_id, timescale: t.mdia.mdhd.timescale.max(1), trak: t.clone() })
        .collect();
    tracks.sort_by_key(|t| t.track_id);
    if tracks.is_empty() {
        return Err(corrupt("init.mp4 has no tracks"));
    }

    let segments = playlist_segments(&playlist);
    let total_us: i64 = segments.iter().map(|(_, d)| (d * 1e6).round() as i64).sum();
    let (from_us, to_us) = (from_ms.saturating_mul(1000), to_ms.saturating_mul(1000));
    if segments.is_empty() || from_us >= total_us {
        return Err(ClipError::BadRange("from_ms is past the end of the recording"));
    }

    // Read only the segments the range touches. Every segment starts on a
    // keyframe, so the keyframe at or before `from` is in the segment that
    // holds `from`.
    let mut files = Vec::new();
    let mut samples: Vec<SampleRef> = Vec::new();
    let mut seg_start_us = 0i64;
    for (name, dur) in &segments {
        let seg_us = (dur * 1e6).round() as i64;
        let (start, end) = (seg_start_us, seg_start_us + seg_us);
        seg_start_us = end;
        if end <= from_us || start >= to_us {
            continue;
        }
        if parse_segment_name(name).is_none() {
            return Err(corrupt(format!("unexpected playlist entry {name}")));
        }
        let path = dir.join(name);
        let data = tokio::fs::read(&path).await?;
        let seg = files.len();
        files.push(path);
        read_segment(&data, &tracks, seg, start, &mut samples)?;
    }

    // Primary track (video, or the only audio): snap back to a keyframe.
    let primary: Vec<&SampleRef> = samples.iter().filter(|s| s.track == 0).collect();
    let start_idx = primary
        .iter()
        .rposition(|s| s.key && s.t_us <= from_us)
        .or_else(|| primary.iter().position(|s| s.key))
        .ok_or(ClipError::BadRange("no keyframe in range"))?;
    let t0 = primary[start_idx].t_us;
    let end_idx = primary.iter().rposition(|s| s.t_us < to_us).unwrap_or(start_idx).max(start_idx);
    let first_p = primary[start_idx].clone();
    let last_p = primary[end_idx].clone();
    let t_end = last_p.t_us + micros(i64::from(last_p.dur), tracks[0].timescale);

    let mut chosen: Vec<SampleRef> = samples
        .into_iter()
        .filter(|s| {
            if s.track == 0 {
                s.t_us >= t0 && (s.seg, s.offset) <= (last_p.seg, last_p.offset)
            } else {
                let end = s.t_us + micros(i64::from(s.dur), tracks[s.track].timescale);
                end > t0 && s.t_us < t_end
            }
        })
        .collect();
    // File order: fragments in sequence, each track's run contiguous.
    chosen.sort_by_key(|s| (s.seg, s.offset));

    let payload: u64 = chosen.iter().map(|s| u64::from(s.size)).sum();
    if payload > MAX_CLIP_BYTES {
        return Err(ClipError::TooLarge);
    }

    // Chunks: runs of one track, contiguous in one file.
    struct Chunk {
        track: usize,
        piece: Piece,
        samples: u32,
    }
    let mut chunks: Vec<Chunk> = Vec::new();
    for s in &chosen {
        if let Some(c) = chunks.last_mut()
            && c.track == s.track
            && c.piece.file == files[s.seg]
            && c.piece.offset + c.piece.len == s.offset
        {
            c.piece.len += u64::from(s.size);
            c.samples += 1;
            continue;
        }
        chunks.push(Chunk {
            track: s.track,
            piece: Piece { file: files[s.seg].clone(), offset: s.offset, len: u64::from(s.size) },
            samples: 1,
        });
    }

    // Presentation start: the keyframe's composition time.
    let p0_us = first_p.t_us + micros(i64::from(first_p.cts.max(0)), tracks[0].timescale);

    let mut out_moov = moov.clone();
    out_moov.mvex = None;
    out_moov.trak.clear();
    let mut movie_ms = 0u64;
    let mut kept: Vec<usize> = Vec::new();
    for (ti, t) in tracks.iter().enumerate() {
        let ss: Vec<&SampleRef> = chosen.iter().filter(|s| s.track == ti).collect();
        if ss.is_empty() {
            continue;
        }
        kept.push(ti);
        let ts = t.timescale;
        let media_dur: u64 = ss.iter().map(|s| u64::from(s.dur)).sum();
        let to_ms = |ticks: i64| -> u64 { (i128::from(ticks.max(0)) * 1000 / i128::from(ts)) as u64 };

        let mut trak = t.trak.clone();
        let stbl = &mut trak.mdia.minf.stbl;
        stbl.stts = Stts {
            entries: run_length(ss.iter().map(|s| s.dur))
                .map(|(v, n)| SttsEntry { sample_count: n, sample_delta: v })
                .collect(),
        };
        let has_cts = ss.iter().any(|s| s.cts != 0);
        stbl.ctts = has_cts.then(|| Ctts {
            entries: run_length(ss.iter().map(|s| s.cts))
                .map(|(v, n)| CttsEntry { sample_count: n, sample_offset: i64::from(v) })
                .collect(),
        });
        let all_key = ss.iter().all(|s| s.key);
        stbl.stss = (!all_key).then(|| Stss {
            entries: ss.iter().enumerate().filter(|(_, s)| s.key).map(|(i, _)| i as u32 + 1).collect(),
        });
        stbl.stsz = Stsz { samples: StszSamples::Different { sizes: ss.iter().map(|s| s.size).collect() } };
        let per_chunk = chunks.iter().filter(|c| c.track == ti).map(|c| c.samples);
        let mut stsc = Vec::new();
        for (i, n) in per_chunk.enumerate() {
            if stsc.last().is_none_or(|e: &StscEntry| e.samples_per_chunk != n) {
                stsc.push(StscEntry { first_chunk: i as u32 + 1, samples_per_chunk: n, sample_description_index: 1 });
            }
        }
        stbl.stsc = Stsc { entries: stsc };
        stbl.stco = Some(Stco::default());
        stbl.co64 = None::<Co64>;

        // Edit list: movie time 0 is the snapped keyframe's presentation.
        let first = ss[0];
        let track_ms;
        let mut entries = Vec::new();
        if ti == 0 {
            let lead = i64::from(first.cts.max(0));
            track_ms = to_ms(media_dur as i64);
            entries.push(ElstEntry { segment_duration: track_ms, media_time: Some(lead as u64), media_rate: 1.into() });
        } else {
            let gap_us = first.t_us - p0_us;
            if gap_us > 0 {
                let empty = (gap_us / 1000) as u64;
                let media = to_ms(media_dur as i64);
                entries.push(ElstEntry { segment_duration: empty, media_time: None, media_rate: 1.into() });
                entries.push(ElstEntry { segment_duration: media, media_time: Some(0), media_rate: 1.into() });
                track_ms = empty + media;
            } else {
                let skip = (i128::from(-gap_us) * i128::from(ts) / 1_000_000) as i64;
                let media = to_ms(media_dur as i64 - skip);
                entries.push(ElstEntry {
                    segment_duration: media,
                    media_time: Some(skip as u64),
                    media_rate: 1.into(),
                });
                track_ms = media;
            }
        }
        trak.edts = Some(Edts { elst: Some(Elst { entries }) });
        trak.tkhd.duration = track_ms;
        trak.mdia.mdhd.duration = media_dur;
        movie_ms = movie_ms.max(track_ms);
        out_moov.trak.push(trak);
    }
    out_moov.mvhd.timescale = 1000;
    out_moov.mvhd.duration = movie_ms;

    let brand_codec: mp4_atom::FourCC =
        if tracks[0].trak.mdia.minf.stbl.stsd.codecs.iter().any(|c| matches!(c, mp4_atom::Codec::Hvc1(_))) {
            b"hvc1".into()
        } else {
            b"avc1".into()
        };
    let ftyp = Ftyp {
        major_brand: b"isom".into(),
        minor_version: 512,
        compatible_brands: vec![b"isom".into(), b"iso2".into(), brand_codec, b"mp41".into()],
    };

    // Chunk offsets depend on the moov size, which does not depend on the
    // offsets' values (stco entries are fixed width): encode once to
    // measure, then with the real offsets.
    let fill = |moov: &mut Moov, base: u64| -> Result<(), ClipError> {
        let mut off = base;
        let mut per_track: Vec<Vec<u32>> = vec![Vec::new(); tracks.len()];
        for c in &chunks {
            per_track[c.track].push(u32::try_from(off).map_err(|_| ClipError::TooLarge)?);
            off += c.piece.len;
        }
        for (trak, &ti) in moov.trak.iter_mut().zip(&kept) {
            trak.mdia.minf.stbl.stco = Some(Stco { entries: std::mem::take(&mut per_track[ti]) });
        }
        Ok(())
    };
    let encode = |moov: &Moov| -> Result<Vec<u8>, ClipError> {
        let mut out = Vec::new();
        ftyp.encode(&mut out).map_err(|e| corrupt(format!("ftyp: {e}")))?;
        moov.encode(&mut out).map_err(|e| corrupt(format!("moov: {e}")))?;
        Ok(out)
    };
    fill(&mut out_moov, 0)?;
    let probe = encode(&out_moov)?.len() as u64;
    fill(&mut out_moov, probe + 8)?;
    let mut header = encode(&out_moov)?;
    if header.len() as u64 != probe {
        return Err(corrupt("moov size changed with offsets"));
    }
    let mdat_size = u32::try_from(payload + 8).map_err(|_| ClipError::TooLarge)?;
    header.extend_from_slice(&mdat_size.to_be_bytes());
    header.extend_from_slice(b"mdat");
    let total = header.len() as u64 + payload;
    if total > MAX_CLIP_BYTES {
        return Err(ClipError::TooLarge);
    }
    Ok(ClipPlan { header, pieces: chunks.into_iter().map(|c| c.piece).collect(), total })
}

/// `(value, count)` runs.
fn run_length<T: PartialEq + Copy>(it: impl Iterator<Item = T>) -> impl Iterator<Item = (T, u32)> {
    let mut runs: Vec<(T, u32)> = Vec::new();
    for v in it {
        match runs.last_mut() {
            Some((last, n)) if *last == v => *n += 1,
            _ => runs.push((v, 1)),
        }
    }
    runs.into_iter()
}

/// Every sample of one segment file we wrote (`moof` + `mdat` pairs, base
/// offsets relative to the `moof`).
fn read_segment(
    data: &[u8],
    tracks: &[TrackIn],
    seg: usize,
    seg_start_us: i64,
    out: &mut Vec<SampleRef>,
) -> Result<(), ClipError> {
    let mut buf = data;
    let first = out.len();
    while !buf.is_empty() {
        let pos = (data.len() - buf.len()) as u64;
        match Any::decode(&mut buf).map_err(|e| corrupt(format!("segment: {e}")))? {
            Any::Moof(moof) => {
                for traf in &moof.traf {
                    let Some(ti) = tracks.iter().position(|t| t.track_id == traf.tfhd.track_id) else { continue };
                    let mut dts = traf.tfdt.as_ref().map_or(0, |t| t.base_media_decode_time as i64);
                    for trun in &traf.trun {
                        let mut off = pos as i64 + i64::from(trun.data_offset.unwrap_or(0));
                        for e in &trun.entries {
                            let size = e.size.unwrap_or(0);
                            let dur = e.duration.unwrap_or(0);
                            let flags = e.flags.unwrap_or(0);
                            if off < 0 || off as u64 + u64::from(size) > data.len() as u64 {
                                return Err(corrupt("sample outside the segment file"));
                            }
                            out.push(SampleRef {
                                track: ti,
                                dts,
                                dur,
                                cts: e.cts.unwrap_or(0),
                                // sample_is_non_sync_sample
                                key: flags & 0x0001_0000 == 0,
                                seg,
                                offset: off as u64,
                                size,
                                t_us: 0,
                            });
                            off += i64::from(size);
                            dts += i64::from(dur);
                        }
                    }
                }
            }
            _ => continue,
        }
    }
    // Timeline: the segment starts at its first primary sample.
    let base = out[first..].iter().find(|s| s.track == 0).map(|s| micros(s.dts, tracks[0].timescale));
    let Some(base) = base else { return Err(corrupt("segment without primary samples")) };
    for s in &mut out[first..] {
        s.t_us = seg_start_us + micros(s.dts, tracks[s.track].timescale) - base;
    }
    Ok(())
}
