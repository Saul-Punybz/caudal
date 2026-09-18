//! The `RustyH264` engine: everything in-process and in safe Rust.
//! `rusty_h264` 0.16 decodes the source (H.264 only), [`crate::scale`]
//! resizes, and one `rusty_h264` encoder per rendition encodes. Built with
//! `default-features = false, features = ["std"]`: its defaults install a
//! process-wide allocator and pull in `rusty_h264-accel` (nasm asm), neither
//! of which a library should impose (and nasm isn't on this Mac anyway).
//!
//! Audio: AAC passes through untouched (same frames, same timestamps); any
//! other audio codec is dropped with a log line (use the `Ffmpeg` engine
//! for Opus sources).
//!
//! The codec runs on its own OS thread per source. If it can't keep up, the
//! queue fills and the feeder skips to the next source keyframe.

use std::collections::BTreeMap;
use std::sync::Arc;

use bytes::Bytes;
use caudal_core::{Codec, Event, Frame, Registry, Subscriber, TrackInfo, VideoParams};
use mp4_atom::{Atom, Avcc};
use rusty_h264::{Decoder, Encoder, EncoderConfig, Preset, YuvFrame};
use tokio::sync::mpsc;

use crate::out::{AUDIO_OUT, RenditionOut, VIDEO_OUT};
use crate::scale::{fit, scale};
use crate::{Rendition, Source, TranscodeConfig};

const QUEUE: usize = 64;
const KEY_INTERVAL_US: i64 = 2_000_000;

enum Item {
    Video(Arc<Frame>),
    Audio(Arc<Frame>),
    Config(TrackInfo),
}

pub(crate) async fn run(registry: &Arc<Registry>, mut sub: Subscriber, src: Source, cfg: &TranscodeConfig) {
    if src.video.codec != Codec::H264 {
        tracing::error!(stream = %src.name, codec = src.video.codec.as_str(), "rusty_h264 only decodes H.264; not transcoding");
        return;
    }
    let aac = src.audio.clone().filter(|a| a.codec == Codec::Aac);
    if let (Some(a), None) = (&src.audio, &aac) {
        tracing::info!(stream = %src.name, codec = a.codec.as_str(),
            "rusty_h264 engine passes AAC through only; renditions carry no audio");
    }
    let mut outs: Vec<RenditionOut> = src
        .renditions
        .iter()
        .map(|r| RenditionOut::new(registry.clone(), format!("{}+{}", src.name, r.label), cfg.buffer, aac.is_some()))
        .collect();
    if let Some(a) = &aac {
        for o in &mut outs {
            o.set_track(a.clone());
        }
    }
    let (tx, rx) = mpsc::channel(QUEUE);
    let worker = {
        let (video, renditions, name) = (src.video.clone(), src.renditions.clone(), src.name.clone());
        std::thread::Builder::new()
            .name(format!("rusty-h264:{}", src.name))
            .spawn(move || Worker::new(video, renditions, outs, name).run(rx))
    };
    let worker = match worker {
        Ok(w) => w,
        Err(e) => {
            tracing::error!(stream = %src.name, error = %e, "cannot start the rusty_h264 thread");
            return;
        }
    };
    let aac_id = aac.as_ref().map(|a| a.id);
    let mut resync = false;
    loop {
        let item = match sub.recv().await {
            Event::End => break,
            Event::Lagged { .. } => {
                resync = true;
                continue;
            }
            Event::TracksChanged => match sub.tracks().into_iter().find(|t| t.codec == Codec::H264) {
                Some(v) => Item::Config(v),
                None => continue,
            },
            Event::Frame(f) if f.track == src.video.id => {
                if resync && !f.keyframe {
                    continue;
                }
                resync = false;
                Item::Video(f)
            }
            Event::Frame(f) if Some(f.track) == aac_id => Item::Audio(f),
            Event::Frame(_) => continue,
        };
        let video = matches!(item, Item::Video(_));
        match tx.try_send(item) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                if video {
                    tracing::debug!(stream = %src.name, "rusty_h264 is behind; skipping to the next keyframe");
                    resync = true;
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::error!(stream = %src.name, "rusty_h264 worker stopped");
                break;
            }
        }
    }
    drop(tx);
    let _ = tokio::task::spawn_blocking(move || worker.join()).await;
}

struct Rung {
    rendition: Rendition,
    size: Option<(usize, usize)>,
    encoder: Option<Encoder>,
    avcc: Option<Vec<u8>>,
}

struct Worker {
    name: String,
    video: TrackInfo,
    sps_pps: (Vec<u8>, Vec<u8>),
    nal_len: usize,
    decoder: Decoder,
    need_key: bool,
    fps: Option<f64>,
    first_dts: Option<i64>,
    bframes: bool,
    /// Decoded pictures by pts, released in display order.
    reorder: BTreeMap<i64, YuvFrame>,
    next_key_us: Option<i64>,
    rungs: Vec<Rung>,
    outs: Vec<RenditionOut>,
}

fn parse_avcc(init: &[u8]) -> Option<(Vec<u8>, Vec<u8>, usize)> {
    let mut buf = init;
    let avcc = Avcc::decode_body(&mut buf).ok()?;
    Some((
        avcc.sequence_parameter_sets.first()?.clone(),
        avcc.picture_parameter_sets.first()?.clone(),
        usize::from(avcc.length_size),
    ))
}

/// NAL units of an Annex B stream, without start codes.
fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push((i, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::with_capacity(starts.len());
    for (k, &(_, begin)) in starts.iter().enumerate() {
        let mut end = starts.get(k + 1).map_or(data.len(), |&(code, _)| code);
        while end > begin && data[end - 1] == 0 {
            end -= 1;
        }
        if end > begin {
            nals.push(&data[begin..end]);
        }
    }
    nals
}

impl Worker {
    fn new(video: TrackInfo, renditions: Vec<Rendition>, outs: Vec<RenditionOut>, name: String) -> Self {
        let (sps, pps, nal_len) = parse_avcc(&video.init).unwrap_or((Vec::new(), Vec::new(), 4));
        let fps = video.video.and_then(|v| v.fps).filter(|f| f.is_finite() && *f > 0.0);
        Self {
            name,
            sps_pps: (sps, pps),
            nal_len,
            video,
            decoder: Decoder::new(),
            need_key: true,
            fps,
            first_dts: None,
            bframes: false,
            reorder: BTreeMap::new(),
            next_key_us: None,
            rungs: renditions
                .into_iter()
                .map(|r| Rung { rendition: r, size: None, encoder: None, avcc: None })
                .collect(),
            outs,
        }
    }

    fn run(mut self, mut rx: mpsc::Receiver<Item>) {
        while let Some(item) = rx.blocking_recv() {
            match item {
                Item::Config(t) => {
                    if let Some((sps, pps, n)) = parse_avcc(&t.init) {
                        self.sps_pps = (sps, pps);
                        self.nal_len = n;
                    }
                    self.video = t;
                }
                Item::Audio(f) => {
                    for o in &mut self.outs {
                        o.push(Frame { track: AUDIO_OUT, ..(*f).clone() });
                    }
                }
                Item::Video(f) => self.video_frame(&f),
            }
        }
        self.drain(0);
    }

    fn video_frame(&mut self, f: &Frame) {
        if self.need_key && !f.keyframe {
            return;
        }
        if self.fps.is_none() {
            match self.first_dts {
                None => self.first_dts = Some(f.dts),
                Some(d0) if f.dts > d0 => {
                    let est = f64::from(self.video.timescale) / (f.dts - d0) as f64;
                    self.fps = Some(if (1.0..=120.0).contains(&est) { est } else { 30.0 });
                }
                _ => {}
            }
        }
        self.bframes |= f.pts != f.dts;
        // AVCC → Annex B, with the parameter sets in front of keyframes.
        let mut au = Vec::with_capacity(f.data.len() + 64);
        if f.keyframe {
            for ps in [&self.sps_pps.0, &self.sps_pps.1] {
                au.extend_from_slice(&[0, 0, 0, 1]);
                au.extend_from_slice(ps);
            }
        }
        let mut rest: &[u8] = &f.data;
        while rest.len() >= self.nal_len {
            let len = rest[..self.nal_len].iter().fold(0usize, |a, &b| (a << 8) | usize::from(b));
            rest = &rest[self.nal_len..];
            if len > rest.len() {
                break;
            }
            au.extend_from_slice(&[0, 0, 0, 1]);
            au.extend_from_slice(&rest[..len]);
            rest = &rest[len..];
        }
        match self.decoder.decode(&au) {
            Ok(Some(pic)) => {
                self.need_key = false;
                self.reorder.insert(f.pts, pic);
            }
            Ok(None) => {}
            Err(e) => {
                tracing::debug!(stream = %self.name, error = ?e, "rusty_h264 decode error; waiting for a keyframe");
                self.need_key = true;
                self.reorder.clear();
            }
        }
        let depth = if self.fps.is_none() {
            usize::MAX
        } else if self.bframes {
            4
        } else {
            0
        };
        self.drain(depth);
    }

    /// Encodes pictures (in pts order) until at most `depth` are held.
    fn drain(&mut self, depth: usize) {
        while self.reorder.len() > depth {
            let Some((pts, pic)) = self.reorder.pop_first() else { break };
            self.encode(pts, &pic);
        }
    }

    fn encode(&mut self, pts: i64, pic: &YuvFrame) {
        let fps = self.fps.unwrap_or(30.0);
        let pts_us = self.video.to_micros(pts);
        // One keyframe clock for every rung, so their IDRs line up.
        let key = self.next_key_us.is_none_or(|k| pts_us >= k);
        if key {
            self.next_key_us = Some(pts_us + KEY_INTERVAL_US);
        }
        let pts90 = (i128::from(pts) * 90_000 / i128::from(self.video.timescale.max(1))) as i64;
        for (rung, out) in self.rungs.iter_mut().zip(self.outs.iter_mut()) {
            let (w, h) = *rung.size.get_or_insert_with(|| fit(pic.width, pic.height, rung.rendition.height as usize));
            if rung.encoder.is_none() {
                let mut c = EncoderConfig::new(w, h);
                c.bitrate = rung.rendition.video_kbps.max(1) * 1000;
                c.framerate = fps as f32;
                c.gop_size = ((fps * 4.0).round() as u32).max(2);
                c.min_keyint = 1;
                c.scenecut = 0;
                c.lookahead = 0;
                c.bframes = 0;
                c.preset = Preset::Fast;
                c.level_idc = 40;
                match Encoder::new(c) {
                    Ok(e) => rung.encoder = Some(e),
                    Err(e) => {
                        tracing::error!(stream = %out.name, error = ?e, "rusty_h264 encoder refused the config");
                        continue;
                    }
                }
            }
            let Some(enc) = rung.encoder.as_mut() else { continue };
            let scaled = if (pic.width, pic.height) == (w, h) { pic.clone() } else { scale(pic, w, h) };
            if key {
                enc.request_keyframe();
            }
            let annexb = match enc.try_encode(&scaled) {
                Ok(b) => b,
                Err(e) => {
                    tracing::debug!(stream = %out.name, error = ?e, "rusty_h264 encode error");
                    continue;
                }
            };
            let (mut sps, mut pps, mut data, mut idr) = (None, None, Vec::new(), false);
            for nal in split_annexb(&annexb) {
                match nal[0] & 0x1F {
                    7 => sps = Some(nal),
                    8 => pps = Some(nal),
                    9 => {}
                    t => {
                        idr |= t == 5;
                        data.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                        data.extend_from_slice(nal);
                    }
                }
            }
            if let (Some(sps), Some(pps)) = (sps, pps) {
                let mut avcc = Vec::new();
                if Avcc::new(sps, pps).is_ok_and(|a| a.encode_body(&mut avcc).is_ok())
                    && rung.avcc.as_ref() != Some(&avcc)
                {
                    rung.avcc = Some(avcc.clone());
                    out.set_track(TrackInfo {
                        id: VIDEO_OUT,
                        codec: Codec::H264,
                        timescale: 90_000,
                        init: Bytes::from(avcc),
                        lang: None,
                        video: Some(VideoParams { width: w as u32, height: h as u32, fps: Some(fps) }),
                        audio: None,
                    });
                }
            }
            if !data.is_empty() {
                out.push(Frame { track: VIDEO_OUT, dts: pts90, pts: pts90, keyframe: idr, data: Bytes::from(data) });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annexb_split_handles_both_start_codes() {
        let s = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 5];
        let nals = split_annexb(&s);
        assert_eq!(nals, [&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 4, 5][..]]);
    }

    /// Encode → decode round trip through the crate as configured here.
    #[test]
    fn encoder_output_decodes() {
        let mut c = EncoderConfig::new(64, 48);
        c.bitrate = 200_000;
        c.scenecut = 0;
        c.lookahead = 0;
        let mut enc = Encoder::new(c).unwrap();
        let mut stream = Vec::new();
        for i in 0..4u8 {
            let mut f = YuvFrame::black(64, 48);
            f.y.iter_mut().enumerate().for_each(|(k, p)| *p = (k as u8).wrapping_add(i * 8));
            let au = enc.try_encode(&f).unwrap();
            assert!(!au.is_empty(), "no buffering expected with lookahead off");
            stream.extend_from_slice(&au);
        }
        let frames = Decoder::new().decode_stream(&stream).unwrap();
        assert_eq!(frames.len(), 4);
        assert_eq!((frames[0].width, frames[0].height), (64, 48));
    }
}
