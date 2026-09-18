//! File readers: turn one MP4/MOV or MPEG-TS file into caudal-core tracks
//! and a pull-based stream of frames in decode order.
//!
//! Everything here is blocking `std::fs` IO; the player calls it through
//! `spawn_blocking`, a small batch at a time. Samples are read one by one
//! from disk (MP4: seek + read by the sample table; TS: 64 KiB chunks), so
//! a file of any size costs its sample table plus one batch in memory.
//!
//! Timestamps are file-relative: video on a 90 kHz clock, audio on its
//! sample rate. Track ids are positions in the returned track list.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use bytes::Bytes;
use caudal_core::{AudioParams, Codec, Frame, TrackId, TrackInfo, TrackKind, VideoParams};
use caudal_ts::demux::{DemuxEvent, Demuxer};
use caudal_ts::ts::{EsUnit, TsDemux};
use mp4_atom::{Any, Atom, Decode, Moov, StszSamples};

const VIDEO_TIMESCALE: u32 = 90_000;
/// A moov larger than this is refused rather than read into memory.
const MAX_MOOV: u64 = 256 * 1024 * 1024;
/// A single sample larger than this means a corrupt table.
const MAX_SAMPLE: u32 = 64 * 1024 * 1024;
const TS_CHUNK: usize = 188 * 348; // ~64 KiB
/// How far into a TS file to look for its tracks before giving up.
const TS_PROBE_BYTES: u64 = 8 * 1024 * 1024;

/// A file ready to play.
pub(crate) struct Opened {
    pub tracks: Vec<TrackInfo>,
    pub duration_secs: Option<f64>,
    pub source: Box<dyn Source>,
}

/// Frames in decode order; `Ok(None)` at the end of the file.
pub(crate) trait Source: Send {
    fn next_frame(&mut self) -> Result<Option<Frame>, String>;
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Container {
    Mp4,
    Ts,
}

fn container_by_extension(path: &Path) -> Option<Container> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "mp4" | "m4v" | "mov" | "m4a" => Some(Container::Mp4),
        "ts" | "m2ts" | "mts" | "m2t" => Some(Container::Ts),
        _ => None,
    }
}

/// Expands the configured items: files stay as they are, a directory
/// becomes its media files (by extension) sorted by name, not recursive.
/// Missing paths are kept, so they surface as a per-item error.
pub(crate) fn expand(items: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for item in items {
        if item.is_dir() {
            let mut files: Vec<PathBuf> = match std::fs::read_dir(item) {
                Ok(rd) => rd
                    .filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.is_file() && container_by_extension(p).is_some())
                    .collect(),
                Err(err) => {
                    tracing::warn!(dir = %item.display(), %err, "channel: cannot list directory");
                    continue;
                }
            };
            files.sort();
            out.extend(files);
        } else {
            out.push(item.clone());
        }
    }
    out
}

/// Opens a file, detecting the container by extension and falling back to
/// sniffing the first bytes.
pub(crate) fn open(path: &Path) -> Result<Opened, String> {
    let mut file = File::open(path).map_err(|e| format!("open: {e}"))?;
    let container = match container_by_extension(path) {
        Some(c) => c,
        None => sniff(&mut file).ok_or("unknown container (expected MP4/MOV or MPEG-TS)")?,
    };
    match container {
        Container::Mp4 => open_mp4(file),
        Container::Ts => open_ts(file),
    }
}

fn sniff(file: &mut File) -> Option<Container> {
    let mut head = [0u8; 189];
    let n = read_up_to(file, &mut head).ok()?;
    file.seek(SeekFrom::Start(0)).ok()?;
    if n == head.len() && head[0] == 0x47 && head[188] == 0x47 {
        return Some(Container::Ts);
    }
    if n >= 8 && matches!(&head[4..8], b"ftyp" | b"moov" | b"mdat" | b"free" | b"wide" | b"skip") {
        return Some(Container::Mp4);
    }
    None
}

fn read_up_to(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

// ---------------------------------------------------------------- MP4 ---

struct Sample {
    offset: u64,
    size: u32,
    dts: i64,
    pts: i64,
    key: bool,
}

struct Mp4Track {
    timescale: u32,
    samples: Vec<Sample>,
    next: usize,
}

struct Mp4Source {
    file: File,
    tracks: Vec<Mp4Track>,
}

/// Walks the top-level boxes by their headers (never reading `mdat`) and
/// decodes `moov` alone.
fn read_moov(file: &mut File) -> Result<Moov, String> {
    let len = file.metadata().map_err(|e| e.to_string())?.len();
    let mut pos = 0u64;
    while pos + 8 <= len {
        file.seek(SeekFrom::Start(pos)).map_err(|e| e.to_string())?;
        let mut h = [0u8; 16];
        let n = read_up_to(file, &mut h).map_err(|e| e.to_string())?;
        if n < 8 {
            break;
        }
        let size32 = u32::from_be_bytes([h[0], h[1], h[2], h[3]]);
        let kind = [h[4], h[5], h[6], h[7]];
        let size = match size32 {
            0 => len - pos,
            1 if n >= 16 => u64::from_be_bytes([h[8], h[9], h[10], h[11], h[12], h[13], h[14], h[15]]),
            1 => return Err("truncated box header".into()),
            s => u64::from(s),
        };
        if size < 8 || pos + size > len {
            return Err(format!("corrupt box {:?} at {pos}", String::from_utf8_lossy(&kind)));
        }
        if &kind == b"moov" {
            if size > MAX_MOOV {
                return Err("moov too large".into());
            }
            file.seek(SeekFrom::Start(pos)).map_err(|e| e.to_string())?;
            let mut buf = vec![0u8; size as usize];
            file.read_exact(&mut buf).map_err(|e| e.to_string())?;
            let mut slice = &buf[..];
            return match Any::decode(&mut slice) {
                Ok(Any::Moov(m)) => Ok(m),
                Ok(_) => Err("moov did not decode as moov".into()),
                Err(e) => Err(format!("moov: {e}")),
            };
        }
        pos += size;
    }
    Err("no moov box (fragmented or truncated MP4 is not supported)".into())
}

/// RFC 7845 `OpusHead` from an ISOBMFF `dOps`, the shape ingest uses.
fn opus_head(o: &mp4_atom::Opus) -> Vec<u8> {
    let d = &o.dops;
    let mut h = b"OpusHead".to_vec();
    h.push(1);
    h.push(d.output_channel_count);
    h.extend_from_slice(&d.pre_skip.to_le_bytes());
    h.extend_from_slice(&d.input_sample_rate.to_le_bytes());
    h.extend_from_slice(&d.output_gain.to_le_bytes());
    h.push(0);
    h
}

fn open_mp4(mut file: File) -> Result<Opened, String> {
    let moov = read_moov(&mut file)?;
    let mut infos = Vec::new();
    let mut tracks = Vec::new();
    let mut duration = 0f64;

    for trak in &moov.trak {
        let stbl = &trak.mdia.minf.stbl;
        let media_ts = trak.mdia.mdhd.timescale.max(1);
        let id = TrackId(infos.len() as u32);
        let encode = |r: Result<(), mp4_atom::Error>| r.map_err(|e| format!("codec config: {e}"));
        let (codec, init, timescale, video, audio) = match stbl.stsd.codecs.first() {
            Some(mp4_atom::Codec::Avc1(a)) => {
                let mut init = Vec::new();
                encode(a.avcc.encode_body(&mut init))?;
                let v = VideoParams { width: a.visual.width.into(), height: a.visual.height.into(), fps: None };
                (Codec::H264, init, VIDEO_TIMESCALE, Some(v), None)
            }
            Some(mp4_atom::Codec::Hvc1(mp4_atom::Hvc1 { hvcc, visual, .. }))
            | Some(mp4_atom::Codec::Hev1(mp4_atom::Hev1 { hvcc, visual, .. })) => {
                let mut init = Vec::new();
                encode(hvcc.encode_body(&mut init))?;
                let v = VideoParams { width: visual.width.into(), height: visual.height.into(), fps: None };
                (Codec::H265, init, VIDEO_TIMESCALE, Some(v), None)
            }
            Some(mp4_atom::Codec::Mp4a(m)) => {
                let Some(dsi) = m.esds.es_desc.dec_config.dec_specific.as_ref() else { continue };
                let rate =
                    if m.audio.sample_rate.integer() > 0 { u32::from(m.audio.sample_rate.integer()) } else { media_ts };
                let a = AudioParams { sample_rate: rate, channels: m.audio.channel_count as u8 };
                (Codec::Aac, dsi.raw.clone(), media_ts, None, Some(a))
            }
            Some(mp4_atom::Codec::Opus(o)) => {
                let a = AudioParams { sample_rate: 48_000, channels: o.audio.channel_count as u8 };
                (Codec::Opus, opus_head(o), media_ts, None, Some(a))
            }
            _ => continue,
        };

        let sizes: Vec<u32> = match &stbl.stsz.samples {
            StszSamples::Identical { count, size } => vec![*size; *count as usize],
            StszSamples::Different { sizes } => sizes.clone(),
        };
        let chunks: Vec<u64> = match (&stbl.stco, &stbl.co64) {
            (Some(s), _) => s.entries.iter().map(|&o| u64::from(o)).collect(),
            (_, Some(c)) => c.entries.clone(),
            _ => return Err("track without chunk offsets".into()),
        };
        let mut offsets = Vec::with_capacity(sizes.len());
        let mut sample = 0usize;
        for (ci, &chunk_off) in chunks.iter().enumerate() {
            let chunk = ci as u32 + 1;
            let per =
                stbl.stsc.entries.iter().rev().find(|e| e.first_chunk <= chunk).map_or(1, |e| e.samples_per_chunk);
            let mut off = chunk_off;
            for _ in 0..per {
                if sample >= sizes.len() {
                    break;
                }
                offsets.push(off);
                off += u64::from(sizes[sample]);
                sample += 1;
            }
        }
        let mut dts = Vec::with_capacity(sizes.len());
        let mut t = 0i64;
        for e in &stbl.stts.entries {
            for _ in 0..e.sample_count {
                dts.push(t);
                t += i64::from(e.sample_delta);
            }
        }
        let mut cts = Vec::with_capacity(sizes.len());
        if let Some(ctts) = &stbl.ctts {
            for e in &ctts.entries {
                for _ in 0..e.sample_count {
                    cts.push(e.sample_offset);
                }
            }
        }
        let keys: Option<&Vec<u32>> = stbl.stss.as_ref().map(|s| &s.entries);
        let count = sizes.len().min(offsets.len()).min(dts.len());
        if count == 0 {
            continue;
        }
        let scale = |v: i64| (i128::from(v) * i128::from(timescale) / i128::from(media_ts)) as i64;
        let mut samples = Vec::with_capacity(count);
        for i in 0..count {
            if sizes[i] > MAX_SAMPLE {
                return Err(format!("sample {i} is {} bytes; table corrupt", sizes[i]));
            }
            let key = video.is_none() || keys.is_none_or(|k| k.binary_search(&(i as u32 + 1)).is_ok());
            samples.push(Sample {
                offset: offsets[i],
                size: sizes[i],
                dts: scale(dts[i]),
                pts: scale(dts[i] + cts.get(i).copied().unwrap_or(0)),
                key,
            });
        }
        duration = duration.max(t as f64 / f64::from(media_ts));
        infos.push(TrackInfo { id, codec, timescale, init: Bytes::from(init), lang: None, video, audio });
        tracks.push(Mp4Track { timescale, samples, next: 0 });
    }
    if infos.is_empty() {
        return Err("no playable tracks (supported: H.264, H.265, AAC, Opus)".into());
    }
    let duration_secs = (duration > 0.0).then_some(duration);
    let infos = sort_tracks(&mut infos, &mut tracks);
    Ok(Opened { tracks: infos, duration_secs, source: Box::new(Mp4Source { file, tracks }) })
}

/// At most one video and one audio track, video first, ids renumbered.
fn sort_tracks<T>(infos: &mut Vec<TrackInfo>, extra: &mut Vec<T>) -> Vec<TrackInfo> {
    let mut pairs: Vec<(TrackInfo, T)> = infos.drain(..).zip(extra.drain(..)).collect();
    let mut kept: Vec<(TrackInfo, T)> = Vec::new();
    for kind in [TrackKind::Video, TrackKind::Audio] {
        if let Some(i) = pairs.iter().position(|(t, _)| t.kind() == kind) {
            kept.push(pairs.swap_remove(i));
        }
    }
    let mut out = Vec::new();
    for (i, (mut info, x)) in kept.into_iter().enumerate() {
        info.id = TrackId(i as u32);
        out.push(info);
        extra.push(x);
    }
    out
}

impl Source for Mp4Source {
    fn next_frame(&mut self) -> Result<Option<Frame>, String> {
        // The track whose next sample is earliest on a common clock.
        let mut best: Option<(i128, usize)> = None;
        for (i, t) in self.tracks.iter().enumerate() {
            if let Some(s) = t.samples.get(t.next) {
                let key = i128::from(s.dts) * 1_000_000 / i128::from(t.timescale);
                if best.is_none_or(|(b, _)| key < b) {
                    best = Some((key, i));
                }
            }
        }
        let Some((_, i)) = best else { return Ok(None) };
        let t = &mut self.tracks[i];
        let s = &t.samples[t.next];
        t.next += 1;
        let mut data = vec![0u8; s.size as usize];
        self.file.seek(SeekFrom::Start(s.offset)).map_err(|e| format!("seek: {e}"))?;
        self.file.read_exact(&mut data).map_err(|e| format!("read sample: {e}"))?;
        Ok(Some(Frame { track: TrackId(i as u32), dts: s.dts, pts: s.pts, keyframe: s.key, data: Bytes::from(data) }))
    }
}

// ----------------------------------------------------------------- TS ---

struct TsSource {
    file: File,
    ts: TsDemux,
    demux: Demuxer,
    units: Vec<EsUnit>,
    events: Vec<DemuxEvent>,
    queue: VecDeque<DemuxEvent>,
    eof: bool,
    /// Our track index for the demuxer's video / audio track.
    video: Option<(u32, Bytes)>,
    audio: Option<(u32, Bytes)>,
}

impl TsSource {
    /// Reads one chunk into `queue`. Returns false at end of file.
    fn fill(&mut self) -> Result<bool, String> {
        if self.eof {
            return Ok(false);
        }
        let mut buf = vec![0u8; TS_CHUNK];
        let n = read_up_to(&mut self.file, &mut buf).map_err(|e| format!("read: {e}"))?;
        self.units.clear();
        if n == 0 {
            self.eof = true;
            self.ts.flush(&mut self.units);
        } else {
            // A trailing partial packet is dropped by the demuxer.
            self.ts.feed(&buf[..n]);
            self.ts.drain(&mut self.units);
        }
        for unit in self.units.drain(..) {
            self.events.clear();
            self.demux.consume(unit, &mut self.events);
            self.queue.extend(self.events.drain(..));
        }
        Ok(n > 0)
    }
}

impl Source for TsSource {
    fn next_frame(&mut self) -> Result<Option<Frame>, String> {
        loop {
            let Some(ev) = self.queue.pop_front() else {
                if !self.fill()? && self.queue.is_empty() {
                    return Ok(None);
                }
                continue;
            };
            match ev {
                DemuxEvent::VideoInit(info) => {
                    if self.video.as_ref().is_some_and(|(_, init)| *init != info.init) {
                        return Err("video parameters changed mid-file".into());
                    }
                }
                DemuxEvent::AudioInit(info) => {
                    if self.audio.as_ref().is_some_and(|(_, init)| *init != info.init) {
                        return Err("audio parameters changed mid-file".into());
                    }
                }
                DemuxEvent::VideoFrame(mut f) => {
                    if let Some((id, _)) = self.video {
                        f.track = TrackId(id);
                        return Ok(Some(f));
                    }
                }
                DemuxEvent::AudioFrame(mut f) => {
                    if let Some((id, _)) = self.audio {
                        f.track = TrackId(id);
                        return Ok(Some(f));
                    }
                }
                // Cues in playlist files are not played out (yet).
                DemuxEvent::Cue(_) => {}
            }
        }
    }
}

fn open_ts(mut file: File) -> Result<Opened, String> {
    let duration_secs = ts_duration(&file);
    // The duration scan moved the (shared) cursor.
    file.seek(SeekFrom::Start(0)).map_err(|e| format!("seek: {e}"))?;
    let mut src = TsSource {
        file,
        ts: TsDemux::new(),
        demux: Demuxer::new(),
        units: Vec::new(),
        events: Vec::new(),
        queue: VecDeque::new(),
        eof: false,
        video: None,
        audio: None,
    };
    // Probe: read until both a video and an audio frame have been seen, or
    // two seconds of one kind without the other, or the probe limit.
    let (mut vinfo, mut ainfo) = (None, None);
    let (mut vfirst, mut afirst): (Option<i64>, Option<i64>) = (None, None);
    let mut read = 0u64;
    'probe: loop {
        let more = src.fill()?;
        read += TS_CHUNK as u64;
        for ev in &src.queue {
            match ev {
                DemuxEvent::VideoInit(i) if vinfo.is_none() => vinfo = Some(i.clone()),
                DemuxEvent::AudioInit(i) if ainfo.is_none() => ainfo = Some(i.clone()),
                DemuxEvent::VideoFrame(f) => {
                    let m = f.dts * 1_000_000 / 90_000;
                    vfirst.get_or_insert(m);
                    if afirst.is_some() || m - vfirst.unwrap_or(m) > 2_000_000 {
                        break 'probe;
                    }
                }
                DemuxEvent::AudioFrame(f) => {
                    let rate = ainfo.as_ref().map_or(48_000, |i: &TrackInfo| i64::from(i.timescale.max(1)));
                    let m = f.dts * 1_000_000 / rate;
                    afirst.get_or_insert(m);
                    if vfirst.is_some() || m - afirst.unwrap_or(m) > 2_000_000 {
                        break 'probe;
                    }
                }
                _ => {}
            }
        }
        if !more || read >= TS_PROBE_BYTES {
            break;
        }
    }
    let mut tracks = Vec::new();
    if let Some(mut v) = vinfo {
        v.id = TrackId(tracks.len() as u32);
        src.video = Some((v.id.0, v.init.clone()));
        tracks.push(v);
    }
    if let Some(mut a) = ainfo {
        a.id = TrackId(tracks.len() as u32);
        src.audio = Some((a.id.0, a.init.clone()));
        tracks.push(a);
    }
    if tracks.is_empty() {
        return Err("no playable tracks in transport stream (supported: H.264, H.265, AAC)".into());
    }
    Ok(Opened { tracks, duration_secs, source: Box::new(src) })
}

/// Duration from the first PTS near the head to the last PTS near the tail
/// (any PID). An estimate for the status API; `None` when unreadable.
fn ts_duration(file: &File) -> Option<f64> {
    const WINDOW: u64 = 1024 * 1024;
    let mut f = file.try_clone().ok()?;
    let len = f.metadata().ok()?.len();
    let mut head = vec![0u8; WINDOW.min(len) as usize];
    f.seek(SeekFrom::Start(0)).ok()?;
    let n = read_up_to(&mut f, &mut head).ok()?;
    head.truncate(n);
    let tail_start = len.saturating_sub(WINDOW);
    let mut tail = vec![0u8; (len - tail_start) as usize];
    f.seek(SeekFrom::Start(tail_start)).ok()?;
    let n = read_up_to(&mut f, &mut tail).ok()?;
    tail.truncate(n);
    let first = pes_pts(&head).into_iter().min()?;
    let last = pes_pts(&tail).into_iter().max()?;
    let mut d = last as i64 - first as i64;
    if d < 0 {
        d += 1 << 33;
    }
    Some(d as f64 / 90_000.0)
}

/// PTS of every PES start in `buf` (after finding packet alignment).
fn pes_pts(buf: &[u8]) -> Vec<u64> {
    let Some(start) = (0..188.min(buf.len())).find(|&i| buf[i] == 0x47 && buf.get(i + 188).is_none_or(|&b| b == 0x47))
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for p in buf[start..].as_chunks::<188>().0 {
        if p[0] != 0x47 || p[1] & 0x40 == 0 {
            continue;
        }
        let afc = (p[3] >> 4) & 0x3;
        let mut i = 4usize;
        if afc & 0x2 != 0 {
            i += 1 + p[4] as usize;
        }
        if afc & 0x1 == 0 || i + 14 > 188 {
            continue;
        }
        let pes = &p[i..];
        if pes[0..3] != [0, 0, 1] || !(0xC0..=0xEF).contains(&pes[3]) || pes[7] & 0x80 == 0 {
            continue;
        }
        let b = &pes[9..14];
        let pts = (u64::from(b[0] >> 1) & 0x7) << 30
            | u64::from(b[1]) << 22
            | u64::from(b[2] >> 1) << 15
            | u64::from(b[3]) << 7
            | u64::from(b[4] >> 1);
        out.push(pts);
    }
    out
}
