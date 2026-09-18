//! A tiny progressive-MP4 demuxer (H.264/H.265 + AAC) for tests and the
//! demo: turns a file ffmpeg wrote into caudal-core tracks and frames, the
//! same shape an RTMP publisher produces. Not part of the library.

#![allow(dead_code)]

use bytes::Bytes;
use caudal_core::{AudioParams, Codec, Frame, TrackId, TrackInfo, VideoParams};
use mp4_atom::{Any, Atom, Decode, Moov, StszSamples};

pub struct Demuxed {
    pub tracks: Vec<TrackInfo>,
    /// Every frame, interleaved by decode time.
    pub frames: Vec<Frame>,
    /// Length of one loop on the video clock (90 kHz), when looping.
    pub video_len: i64,
}

const VIDEO_TIMESCALE: u32 = 90_000;

pub fn demux(file: &[u8]) -> Demuxed {
    let mut buf = file;
    let mut moov = None;
    while !buf.is_empty() {
        match Any::decode(&mut buf).expect("mp4 atom") {
            Any::Moov(m) => moov = Some(m),
            _ => continue,
        }
    }
    let moov: Moov = moov.expect("moov");
    let mut tracks = Vec::new();
    let mut frames: Vec<(i64, Frame)> = Vec::new();
    let mut video_len = 0;

    for trak in &moov.trak {
        let stbl = &trak.mdia.minf.stbl;
        let ts = trak.mdia.mdhd.timescale;
        let id = TrackId(tracks.len() as u32);
        let (codec, init, timescale, video, audio) = match stbl.stsd.codecs.first() {
            Some(mp4_atom::Codec::Avc1(a)) => {
                let mut init = Vec::new();
                a.avcc.encode_body(&mut init).unwrap();
                let v = VideoParams { width: a.visual.width.into(), height: a.visual.height.into(), fps: None };
                (Codec::H264, init, VIDEO_TIMESCALE, Some(v), None)
            }
            Some(mp4_atom::Codec::Hvc1(h)) => {
                let mut init = Vec::new();
                h.hvcc.encode_body(&mut init).unwrap();
                let v = VideoParams { width: h.visual.width.into(), height: h.visual.height.into(), fps: None };
                (Codec::H265, init, VIDEO_TIMESCALE, Some(v), None)
            }
            Some(mp4_atom::Codec::Mp4a(m)) => {
                let asc = m.esds.es_desc.dec_config.dec_specific.as_ref().expect("asc").raw.clone();
                let a = AudioParams { sample_rate: ts, channels: m.audio.channel_count as u8 };
                (Codec::Aac, asc, ts, None, Some(a))
            }
            _ => continue,
        };
        let is_video = video.is_some();
        tracks.push(TrackInfo { id, codec, timescale, init: Bytes::from(init), lang: None, video, audio });

        // Sample table → (offset, size, dts, cts, key).
        let sizes: Vec<u32> = match &stbl.stsz.samples {
            StszSamples::Identical { count, size } => vec![*size; *count as usize],
            StszSamples::Different { sizes } => sizes.clone(),
        };
        let chunks: Vec<u64> = match (&stbl.stco, &stbl.co64) {
            (Some(s), _) => s.entries.iter().map(|&o| o.into()).collect(),
            (_, Some(c)) => c.entries.clone(),
            _ => panic!("no chunk offsets"),
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
        let keys: Option<Vec<u32>> = stbl.stss.as_ref().map(|s| s.entries.clone());
        let scale = |v: i64| i64::try_from(i128::from(v) * i128::from(timescale) / i128::from(ts)).unwrap();
        if is_video {
            video_len = scale(t);
        }
        for i in 0..sizes.len() {
            let off = offsets[i] as usize;
            let data = Bytes::copy_from_slice(&file[off..off + sizes[i] as usize]);
            let d = scale(dts[i]);
            let p = scale(dts[i] + cts.get(i).copied().unwrap_or(0));
            let key = keys.as_ref().is_none_or(|k| k.contains(&(i as u32 + 1)));
            let micros = d * 1_000_000 / i64::from(timescale);
            frames.push((micros, Frame { track: id, dts: d, pts: p, keyframe: key, data }));
        }
    }
    frames.sort_by_key(|(m, f)| (*m, f.track));
    Demuxed { tracks, frames: frames.into_iter().map(|(_, f)| f).collect(), video_len }
}

impl Demuxed {
    /// The frames of loop number `n`, shifted so loops follow each other
    /// seamlessly on the video clock. Audio past the video's end is dropped.
    pub fn looped(&self, n: i64) -> impl Iterator<Item = Frame> + '_ {
        let len_micros = self.video_len * 1_000_000 / i64::from(VIDEO_TIMESCALE);
        self.frames.iter().filter_map(move |f| {
            let ts = i64::from(self.tracks[f.track.0 as usize].timescale);
            let len = len_micros * ts / 1_000_000;
            if f.dts >= len {
                return None;
            }
            let mut f = f.clone();
            f.dts += n * len;
            f.pts += n * len;
            Some(f)
        })
    }

    pub fn micros(&self, f: &Frame) -> i64 {
        f.dts * 1_000_000 / i64::from(self.tracks[f.track.0 as usize].timescale)
    }
}
