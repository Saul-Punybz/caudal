//! H.264 → I420 → VMX for one output.
//!
//! 8-bit 4:2:0 H.264 (Baseline, Main, High; CAVLC or CABAC; B-frames)
//! decodes in process with `rusty_h264` (the decoder Caudal's `RustyH264`
//! transcode engine uses), which returns pictures in decode order; they
//! are held by pts and released in display order. Other H.264 (High 10,
//! 4:2:2, 4:4:4) goes through ffmpeg: Annex B on stdin, raw I420 at the
//! track's size on stdout, read by a thread of its own. ffmpeg emits
//! pictures in display order, so each one takes the smallest pts not yet
//! emitted.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};
use std::fs::File;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use caudal_core::{Codec, Frame, TrackInfo};
use caudal_transcode::pipe::FfmpegProcess;
use mp4_atom::{Atom, Avcc};
use open_media_transport::sender::{Sender, VideoParams};
use rusty_h264::{Decoder, YuvFrame};
use vmx_codec::{PixelFormat, Plane};

use super::{Ctx, OutputStats, frame_rate};
use crate::time::micros_to_omt;

/// `profile_idc`s `rusty_h264` decodes (8-bit 4:2:0): Baseline, Main,
/// Extended, High.
const RUSTY_PROFILES: [u8; 4] = [66, 77, 88, 100];
/// Pictures held for reordering once the stream has shown B-frames (pts
/// differs from dts); the DPB of any 4:2:0 level-4.x stream at 1080p is
/// at most 4 frames.
const REORDER_DEPTH: usize = 4;
/// Most pts kept for pictures ffmpeg has not emitted yet.
const MAX_PENDING: usize = 64;
/// An access unit delimiter, written after each access unit so ffmpeg's
/// parser knows the unit is complete without waiting for the next one.
const AUD: [u8; 6] = [0, 0, 0, 1, 9, 0xF0];

/// Frame rate and aspect ratio, shared with ffmpeg's reader thread.
#[derive(Clone, Copy)]
struct Shape {
    rate: (i32, i32),
}

fn params(shape: Shape, width: usize, height: usize) -> VideoParams {
    VideoParams {
        frame_rate_n: shape.rate.0,
        frame_rate_d: shape.rate.1,
        aspect_ratio: width as f32 / height.max(1) as f32,
        // Undefined: receivers pick BT.601 below 720 lines, BT.709 above.
        color_space: 0,
        premultiplied: false,
    }
}

/// Sends one I420 picture (tightly packed planes of an even-sized frame).
fn send_i420(
    sender: &Sender,
    stats: &OutputStats,
    name: &str,
    (w, h): (usize, usize),
    planes: [Vec<u8>; 3],
    pts_us: i64,
    shape: Shape,
) {
    let [y, u, v] = planes;
    let planes = vec![Plane { data: y, stride: w }, Plane { data: u, stride: w / 2 }, Plane { data: v, stride: w / 2 }];
    let frame = match vmx_codec::Frame::from_planes(w, h, PixelFormat::I420, planes) {
        Ok(f) => f,
        Err(e) => {
            tracing::debug!(output = %name, error = %e, "bad I420 picture");
            OutputStats::add(&stats.errors, 1);
            return;
        }
    };
    match sender.send_video(&frame, params(shape, w, h), micros_to_omt(pts_us), &[]) {
        Ok(_) => OutputStats::add(&stats.video_sent, 1),
        Err(e) => {
            tracing::debug!(output = %name, error = %e, "OMT sender refused a picture");
            OutputStats::add(&stats.errors, 1);
        }
    }
}

/// A decoded picture as even-sized, tightly packed I420 planes (VMX needs
/// an even width; 4:2:0 wants an even height). Odd sizes lose their last
/// column/row.
fn even_planes(pic: YuvFrame) -> Option<((usize, usize), [Vec<u8>; 3])> {
    let (w, h) = (pic.width, pic.height);
    let (ew, eh) = (w & !1, h & !1);
    if ew == 0 || eh == 0 {
        return None;
    }
    if (ew, eh) == (w, h) {
        if pic.y.len() < w * h || pic.u.len() < w * h / 4 || pic.v.len() < w * h / 4 {
            return None;
        }
        return Some(((w, h), [pic.y, pic.u, pic.v]));
    }
    let crop = |src: &[u8], stride: usize, cw: usize, ch: usize| -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(cw * ch);
        for r in 0..ch {
            out.extend_from_slice(src.get(r * stride..r * stride + cw)?);
        }
        Some(out)
    };
    let cstride = w.div_ceil(2);
    let y = crop(&pic.y, w, ew, eh)?;
    let u = crop(&pic.u, cstride, ew / 2, eh / 2)?;
    let v = crop(&pic.v, cstride, ew / 2, eh / 2)?;
    Some(((ew, eh), [y, u, v]))
}

/// The parameter sets and NAL length size of an avcC.
struct AvcConfig {
    profile: u8,
    /// 8-bit 4:2:0 (the avcC extension says so, or has none).
    yuv420_8bit: bool,
    sps: Vec<Vec<u8>>,
    pps: Vec<Vec<u8>>,
    nal_len: usize,
}

fn parse_avcc(init: &[u8]) -> Option<AvcConfig> {
    let mut buf = init;
    let a = Avcc::decode_body(&mut buf).ok()?;
    let nal_len = usize::from(a.length_size);
    if !(1..=4).contains(&nal_len) {
        return None;
    }
    let yuv420_8bit =
        a.ext.as_ref().is_none_or(|e| e.chroma_format == 1 && e.bit_depth_luma == 8 && e.bit_depth_chroma == 8);
    Some(AvcConfig {
        profile: a.avc_profile_indication,
        yuv420_8bit,
        sps: a.sequence_parameter_sets,
        pps: a.picture_parameter_sets,
        nal_len,
    })
}

/// AVCC access unit → Annex B, with the parameter sets in front of
/// keyframes.
fn to_annexb(cfg: &AvcConfig, f: &Frame, out: &mut Vec<u8>) {
    out.clear();
    if f.keyframe {
        for ps in cfg.sps.iter().chain(&cfg.pps) {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(ps);
        }
    }
    let mut rest: &[u8] = &f.data;
    while rest.len() >= cfg.nal_len {
        let len = rest[..cfg.nal_len].iter().fold(0usize, |a, &b| (a << 8) | usize::from(b));
        rest = &rest[cfg.nal_len..];
        if len > rest.len() {
            break;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&rest[..len]);
        rest = &rest[len..];
    }
}

/// ffmpeg decoding one run of the stream (started at a keyframe).
struct FfmpegDecoder {
    stdin: Option<File>,
    proc: Option<FfmpegProcess>,
    reader: Option<std::thread::JoinHandle<()>>,
    pending: Arc<Mutex<BinaryHeap<Reverse<i64>>>>,
}

impl FfmpegDecoder {
    fn start(ctx: &Ctx, track: &TrackInfo, (w, h): (usize, usize), shape: &Arc<Mutex<Shape>>) -> std::io::Result<Self> {
        let args: Vec<String> = [
            "-hide_banner",
            "-loglevel",
            "error",
            "-probesize",
            "32",
            "-analyzeduration",
            "0",
            // Not `-fflags nobuffer`: with it ffmpeg held ~64 pictures (2 s)
            // of a piped H.264 stream; without, 4 (the B-frame reorder).
            "-threads",
            "1",
            "-f",
            "h264",
            "-i",
            "pipe:0",
            "-map",
            "0:v:0",
            "-fps_mode",
            "passthrough",
            "-s",
            &format!("{w}x{h}"),
            "-pix_fmt",
            "yuv420p",
            "-f",
            "rawvideo",
            "pipe:1",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let (proc, stdin, stdout) = ctx.spawn_ffmpeg(&args)?;
        let pending: Arc<Mutex<BinaryHeap<Reverse<i64>>>> = Arc::default();
        let reader = {
            let (sender, stats, name) = (ctx.sender.clone(), ctx.stats.clone(), ctx.name.clone());
            let (pending, shape, track) = (pending.clone(), shape.clone(), track.clone());
            std::thread::Builder::new()
                .name(format!("omt-out-ff:{}", ctx.name))
                .spawn(move || read_pictures(stdout, (w, h), &track, &pending, &shape, &sender, &stats, &name))?
        };
        Ok(FfmpegDecoder { stdin: Some(stdin), proc: Some(proc), reader: Some(reader), pending })
    }

    fn write(&mut self, pts: i64, au: &[u8]) -> std::io::Result<()> {
        {
            let mut p = self.pending.lock().expect("poisoned");
            p.push(Reverse(pts));
            if p.len() > MAX_PENDING {
                // ffmpeg dropped pictures: forget the oldest pts.
                p.pop();
            }
        }
        let stdin = self.stdin.as_mut().ok_or_else(|| std::io::Error::other("closed"))?;
        stdin.write_all(au)?;
        stdin.write_all(&AUD)
    }
}

impl Drop for FfmpegDecoder {
    fn drop(&mut self) {
        drop(self.stdin.take());
        // Kills the process group; its stdout closes and the reader ends.
        drop(self.proc.take());
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn read_pictures(
    mut stdout: File,
    (w, h): (usize, usize),
    track: &TrackInfo,
    pending: &Mutex<BinaryHeap<Reverse<i64>>>,
    shape: &Mutex<Shape>,
    sender: &Sender,
    stats: &OutputStats,
    name: &str,
) {
    let (ys, cs) = (w * h, w * h / 4);
    let mut last_pts: Option<i64> = None;
    loop {
        let mut planes = [vec![0u8; ys], vec![0u8; cs], vec![0u8; cs]];
        for p in &mut planes {
            if stdout.read_exact(p).is_err() {
                return;
            }
        }
        let t0 = Instant::now();
        let pts = match pending.lock().expect("poisoned").pop() {
            Some(Reverse(p)) => p,
            // More pictures than access units: carry on one frame later.
            None => {
                let s = *shape.lock().expect("poisoned");
                let step = i64::from(track.timescale) * i64::from(s.rate.1) / i64::from(s.rate.0.max(1));
                last_pts.map_or(0, |l| l + step.max(1))
            }
        };
        last_pts = Some(pts);
        let s = *shape.lock().expect("poisoned");
        send_i420(sender, stats, name, (w, h), planes, track.to_micros(pts), s);
        OutputStats::add(&stats.encode_nanos, t0.elapsed().as_nanos() as u64);
    }
}

enum Engine {
    Rusty {
        dec: Box<Decoder>,
        /// Decoded pictures by pts, released in display order.
        reorder: BTreeMap<i64, YuvFrame>,
        bframes: bool,
    },
    /// Started at the first keyframe with receivers; `None` while idle.
    Ffmpeg(Option<FfmpegDecoder>),
    /// Logged when chosen; frames are ignored.
    Unsupported,
}

/// The video half of one output.
pub(crate) struct VideoOut {
    track: TrackInfo,
    cfg: Option<AvcConfig>,
    engine: Engine,
    /// The decoder must (re)start at a keyframe.
    need_key: bool,
    shape: Arc<Mutex<Shape>>,
    rate_known: bool,
    last_dts: Option<i64>,
    au: Vec<u8>,
}

impl VideoOut {
    pub fn new(track: TrackInfo, ctx: &Ctx) -> Self {
        let cfg = (track.codec == Codec::H264).then(|| parse_avcc(&track.init)).flatten();
        let engine = match (&cfg, track.codec) {
            (_, c) if c != Codec::H264 => {
                tracing::warn!(output = %ctx.name, codec = c.as_str(),
                    "OMT output sends H.264 sources only; this output carries audio only");
                Engine::Unsupported
            }
            (None, _) => {
                tracing::warn!(output = %ctx.name, "H.264 track without a usable avcC; this output carries audio only");
                Engine::Unsupported
            }
            (Some(c), _)
                if RUSTY_PROFILES.contains(&c.profile) && c.yuv420_8bit && !ctx.options.decode_with_ffmpeg =>
            {
                Engine::Rusty { dec: Box::new(Decoder::new()), reorder: BTreeMap::new(), bframes: false }
            }
            (Some(c), _) if ctx.options.ffmpeg.is_some() => {
                tracing::info!(output = %ctx.name, profile = c.profile, "decoding H.264 with ffmpeg");
                Engine::Ffmpeg(None)
            }
            (Some(c), _) => {
                tracing::warn!(output = %ctx.name, profile = c.profile,
                    "this H.264 profile needs ffmpeg ([omt] ffmpeg); this output carries audio only");
                Engine::Unsupported
            }
        };
        let fps = track.video.and_then(|v| v.fps).filter(|f| f.is_finite() && *f > 0.0);
        VideoOut {
            shape: Arc::new(Mutex::new(Shape { rate: fps.map_or((30, 1), frame_rate) })),
            rate_known: fps.is_some(),
            track,
            cfg,
            engine,
            need_key: true,
            last_dts: None,
            au: Vec::new(),
        }
    }

    pub fn track(&self) -> &TrackInfo {
        &self.track
    }

    /// Forgets decoder state; the next picture decoded is a keyframe.
    fn idle(&mut self) {
        self.need_key = true;
        match &mut self.engine {
            Engine::Rusty { dec, reorder, .. } => {
                **dec = Decoder::new();
                reorder.clear();
            }
            Engine::Ffmpeg(f) => *f = None,
            Engine::Unsupported => {}
        }
    }

    pub fn push(&mut self, f: &Frame, ctx: &Ctx) {
        let stats = &ctx.stats;
        OutputStats::add(&stats.video_in, 1);
        if matches!(self.engine, Engine::Unsupported) {
            return;
        }
        self.estimate_rate(f.dts);
        if ctx.sender.video_receivers() == 0 {
            if !self.need_key {
                self.idle();
            }
            OutputStats::add(&stats.video_skipped, 1);
            return;
        }
        if self.need_key && !f.keyframe {
            OutputStats::add(&stats.video_skipped, 1);
            return;
        }
        self.need_key = false;
        let Some(cfg) = &self.cfg else { return };
        to_annexb(cfg, f, &mut self.au);
        let t0 = Instant::now();
        match &mut self.engine {
            Engine::Rusty { dec, reorder, bframes } => {
                *bframes |= f.pts != f.dts;
                match dec.decode(&self.au) {
                    Ok(Some(pic)) => {
                        reorder.insert(f.pts, pic);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::debug!(output = %ctx.name, error = ?e, "rusty_h264 decode error; waiting for a keyframe");
                        OutputStats::add(&stats.errors, 1);
                        self.idle();
                        return;
                    }
                }
                let depth = if *bframes { REORDER_DEPTH } else { 0 };
                let shape = *self.shape.lock().expect("poisoned");
                while reorder.len() > depth {
                    let Some((pts, pic)) = reorder.pop_first() else { break };
                    if let Some((size, planes)) = even_planes(pic) {
                        send_i420(&ctx.sender, stats, &ctx.name, size, planes, self.track.to_micros(pts), shape);
                    }
                }
                OutputStats::add(&stats.encode_nanos, t0.elapsed().as_nanos() as u64);
            }
            Engine::Ffmpeg(slot) => {
                if slot.is_none() {
                    let size = self.track.video.map(|v| (v.width as usize & !1, v.height as usize & !1));
                    let Some(size) = size.filter(|&(w, h)| w > 0 && h > 0) else {
                        tracing::warn!(output = %ctx.name, "H.264 track without a size; cannot decode it");
                        self.engine = Engine::Unsupported;
                        return;
                    };
                    match FfmpegDecoder::start(ctx, &self.track, size, &self.shape) {
                        Ok(d) => *slot = Some(d),
                        Err(e) => {
                            tracing::warn!(output = %ctx.name, error = %e, "cannot start ffmpeg; retrying at the next keyframe");
                            OutputStats::add(&stats.errors, 1);
                            self.need_key = true;
                            return;
                        }
                    }
                }
                if let Some(d) = slot
                    && let Err(e) = d.write(f.pts, &self.au)
                {
                    tracing::warn!(output = %ctx.name, error = %e, "ffmpeg stopped; restarting at the next keyframe");
                    OutputStats::add(&stats.errors, 1);
                    self.idle();
                }
            }
            Engine::Unsupported => {}
        }
    }

    /// Takes the frame rate from the first dts step when the track does
    /// not declare one.
    fn estimate_rate(&mut self, dts: i64) {
        if self.rate_known {
            return;
        }
        if let Some(prev) = self.last_dts
            && dts > prev
        {
            let est = f64::from(self.track.timescale) / (dts - prev) as f64;
            let fps = if (1.0..=240.0).contains(&est) { est } else { 30.0 };
            self.shape.lock().expect("poisoned").rate = frame_rate(fps);
            self.rate_known = true;
        }
        self.last_dts = Some(dts);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn odd_pictures_are_cropped_to_even() {
        let pic = YuvFrame {
            width: 5,
            height: 3,
            y: (0..15).collect(),
            u: vec![100, 101, 102, 103, 104, 105],
            v: vec![200, 201, 202, 203, 204, 205],
        };
        let ((w, h), [y, u, v]) = even_planes(pic).unwrap();
        assert_eq!((w, h), (4, 2));
        assert_eq!(y, [0, 1, 2, 3, 5, 6, 7, 8]);
        assert_eq!((u, v), (vec![100, 101], vec![200, 201]));
    }

    #[test]
    fn short_planes_are_refused() {
        let pic = YuvFrame { width: 4, height: 2, y: vec![0; 3], u: vec![0; 2], v: vec![0; 2] };
        assert!(even_planes(pic).is_none());
    }

    #[test]
    fn avcc_to_annexb_prepends_parameter_sets_on_keyframes() {
        let cfg = AvcConfig { profile: 66, yuv420_8bit: true, sps: vec![vec![0x67, 1]], pps: vec![vec![0x68, 2]], nal_len: 4 };
        let mut data = Vec::new();
        data.extend_from_slice(&[0, 0, 0, 2, 0x65, 9]);
        data.extend_from_slice(&[0, 0, 0, 9]); // truncated NAL: ignored
        let f = Frame {
            track: caudal_core::TrackId(0),
            dts: 0,
            pts: 0,
            keyframe: true,
            data: bytes::Bytes::from(data),
        };
        let mut out = Vec::new();
        to_annexb(&cfg, &f, &mut out);
        assert_eq!(out, [0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 9]);
    }
}
