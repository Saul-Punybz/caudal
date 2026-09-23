//! Raw frames in, H.264 + AAC stream out: one ffmpeg per [`Feed`], fed
//! uncompressed video and 32-bit float PCM on stdin as live Matroska
//! (`V_UNCOMPRESSED` with a FourCC + `A_PCM/FLOAT/IEEE`, see `NOTES.md` for
//! why that container), read back as MPEG-TS through `caudal-ts` and
//! published into the registry under [`FeedConfig::stream`].
//!
//! The process handling (own process group killed on drop, stderr logged,
//! TS demux, lazy publish, clock shift) is `caudal_transcode::pipe`, shared
//! with the transcode engine.
//!
//! Timestamps in: video on 90 kHz, audio on its sample rate, both from the
//! same origin (what [`crate::time::TimeMap`] produces). Frames come out of
//! ffmpeg with exactly those timestamps (`-copyts`), so the published
//! stream is on the caller's clock.
//!
//! Lifetime: ffmpeg starts at the first video frame and is restarted (the
//! published stream stays) when the video or audio format changes, and with
//! backoff if it dies. [`Feed::finish`] lets ffmpeg flush and ends the
//! stream; dropping the [`Feed`] kills ffmpeg and ends the stream at once.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use caudal_core::{BufferConfig, Registry, TrackId};
use caudal_transcode::pipe::{Clock, FfmpegProcess, MkvTrack, MkvWriter, RenditionOut, START_PID, read_ts_outputs};
use tokio::io::AsyncWriteExt;
use tokio::process::ChildStdin;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::time::VIDEO_HZ;

/// Raw video frames queued for ffmpeg (≈ 100 ms at 60 fps; 25 MB of
/// 1080p UYVY). When full, pushes fail with [`PushError::Full`].
const VIDEO_QUEUE: usize = 6;
/// Audio chunks queued for ffmpeg.
const AUDIO_QUEUE: usize = 64;
const MIN_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(10);
/// A session that ran this long resets the backoff.
const HEALTHY_RUN: Duration = Duration::from_secs(20);
/// How long ffmpeg gets to flush after the input ends.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(3);
/// How long to wait for the first audio chunk before starting ffmpeg
/// without audio (when audio is expected).
const AUDIO_WAIT: Duration = Duration::from_millis(300);
/// Matroska TimecodeScale: microseconds.
const MKV_SCALE_NS: u32 = 1_000;
const VIDEO_IN: TrackId = TrackId(0);
const AUDIO_IN: TrackId = TrackId(1);

#[derive(Debug, Clone)]
pub struct FeedConfig {
    /// ffmpeg binary.
    pub ffmpeg: PathBuf,
    /// Name the H.264/AAC stream is published under.
    pub stream: String,
    pub buffer: BufferConfig,
    pub video_kbps: u32,
    pub audio_kbps: u32,
    /// Hold back the stream (up to 300 frames) until AAC is described, and
    /// give the first audio chunk 300 ms to arrive before starting.
    pub expect_audio: bool,
}

/// Pixel layout of a raw frame, tightly packed (no row padding).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelLayout {
    /// Packed 4:2:2, `U0 Y0 V0 Y1` (what VMX decodes to).
    Uyvy,
    /// 4:2:0, Y plane then interleaved UV.
    Nv12,
    /// 4:2:0, Y, U, V planes.
    I420,
}

impl PixelLayout {
    /// The FourCC ffmpeg's Matroska demuxer maps to this layout.
    pub fn fourcc(self) -> [u8; 4] {
        match self {
            PixelLayout::Uyvy => *b"UYVY",
            PixelLayout::Nv12 => *b"NV12",
            PixelLayout::I420 => *b"I420",
        }
    }

    /// Bytes of one `width` x `height` frame, or `None` for odd or zero
    /// dimensions.
    pub fn frame_len(self, width: u32, height: u32) -> Option<usize> {
        if width == 0 || height == 0 || width % 2 == 1 || height % 2 == 1 || width > 16384 || height > 16384 {
            return None;
        }
        let (w, h) = (width as usize, height as usize);
        Some(match self {
            PixelLayout::Uyvy => w * h * 2,
            PixelLayout::Nv12 | PixelLayout::I420 => w * h * 3 / 2,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoFormat {
    pub width: u32,
    pub height: u32,
    pub layout: PixelLayout,
    /// Frame rate `fps_num / fps_den` (drives the GOP length and the
    /// track's declared fps).
    pub fps_num: u32,
    pub fps_den: u32,
}

impl VideoFormat {
    fn fps(&self) -> f64 {
        let f = f64::from(self.fps_num) / f64::from(self.fps_den.max(1));
        if f.is_finite() && (1.0..=240.0).contains(&f) { f } else { 30.0 }
    }
}

/// One whole raw picture.
#[derive(Debug, Clone)]
pub struct VideoFrame {
    pub format: VideoFormat,
    /// Presentation time, 90 kHz.
    pub pts: i64,
    /// Exactly `format.layout.frame_len(width, height)` bytes.
    pub data: Bytes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleLayout {
    /// `L R L R ...`
    Interleaved,
    /// All of channel 0, then all of channel 1, ... (OMT's layout).
    Planar,
}

/// A chunk of 32-bit float little-endian PCM.
#[derive(Debug, Clone)]
pub struct AudioFrame {
    pub sample_rate: u32,
    pub channels: u8,
    pub layout: SampleLayout,
    /// Presentation time of the first sample, in `sample_rate` ticks.
    pub pts: i64,
    /// `samples * channels * 4` bytes.
    pub data: Bytes,
}

impl AudioFrame {
    /// Samples per channel, if the chunk is well formed.
    pub fn samples(&self) -> Option<usize> {
        let per = usize::from(self.channels) * 4;
        (self.sample_rate > 0 && per > 0 && !self.data.is_empty() && self.data.len().is_multiple_of(per))
            .then(|| self.data.len() / per)
    }

    fn format(&self) -> (u32, u8) {
        (self.sample_rate, self.channels)
    }

    /// The samples interleaved (planar input is reordered).
    fn interleaved(&self) -> Bytes {
        let ch = usize::from(self.channels);
        if self.layout == SampleLayout::Interleaved || ch == 1 {
            return self.data.clone();
        }
        let n = self.data.len() / (ch * 4);
        let mut out = vec![0u8; self.data.len()];
        for c in 0..ch {
            let plane = &self.data[c * n * 4..(c + 1) * n * 4];
            for (i, s) in plane.as_chunks::<4>().0.iter().enumerate() {
                let o = (i * ch + c) * 4;
                out[o..o + 4].copy_from_slice(s);
            }
        }
        Bytes::from(out)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushError {
    /// ffmpeg is behind and the queue is full: the frame was dropped.
    Full,
    /// The feed is gone.
    Closed,
    /// The frame's size doesn't match its format (or the format is bad).
    Invalid,
}

impl std::fmt::Display for PushError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PushError::Full => "feed queue full",
            PushError::Closed => "feed closed",
            PushError::Invalid => "frame does not match its format",
        })
    }
}

impl std::error::Error for PushError {}

fn check_video(f: &VideoFrame) -> Result<(), PushError> {
    let v = &f.format;
    match v.layout.frame_len(v.width, v.height) {
        Some(n) if n == f.data.len() && v.fps_num > 0 && v.fps_den > 0 => Ok(()),
        _ => Err(PushError::Invalid),
    }
}

fn check_audio(a: &AudioFrame) -> Result<(), PushError> {
    if a.samples().is_some() && a.sample_rate <= crate::time::MAX_SAMPLE_RATE {
        Ok(())
    } else {
        Err(PushError::Invalid)
    }
}

/// A running raw-frame → H.264/AAC feed. Dropping it kills ffmpeg and ends
/// the published stream.
pub struct Feed {
    video: mpsc::Sender<VideoFrame>,
    audio: mpsc::Sender<AudioFrame>,
    task: Option<JoinHandle<()>>,
}

impl Feed {
    /// Starts the feed (ffmpeg starts at the first video frame). Must be
    /// called inside a tokio runtime.
    pub fn start(registry: Arc<Registry>, cfg: FeedConfig) -> std::io::Result<Feed> {
        tokio::runtime::Handle::try_current().map_err(|_| std::io::Error::other("omt feed: no tokio runtime"))?;
        if !caudal_core::media::valid_stream_name(&cfg.stream) {
            return Err(std::io::Error::other(format!("omt feed: bad stream name `{}`", cfg.stream)));
        }
        let (video, vrx) = mpsc::channel(VIDEO_QUEUE);
        let (audio, arx) = mpsc::channel(AUDIO_QUEUE);
        let task = tokio::spawn(run(registry, cfg, vrx, arx));
        Ok(Feed { video, audio, task: Some(task) })
    }

    /// Queues a video frame without waiting (callable from any thread).
    pub fn try_push_video(&self, f: VideoFrame) -> Result<(), PushError> {
        check_video(&f)?;
        self.video.try_send(f).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => PushError::Full,
            mpsc::error::TrySendError::Closed(_) => PushError::Closed,
        })
    }

    /// Queues an audio chunk without waiting (callable from any thread).
    pub fn try_push_audio(&self, a: AudioFrame) -> Result<(), PushError> {
        check_audio(&a)?;
        self.audio.try_send(a).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => PushError::Full,
            mpsc::error::TrySendError::Closed(_) => PushError::Closed,
        })
    }

    /// Queues a video frame, waiting for room.
    pub async fn push_video(&self, f: VideoFrame) -> Result<(), PushError> {
        check_video(&f)?;
        self.video.send(f).await.map_err(|_| PushError::Closed)
    }

    /// Queues an audio chunk, waiting for room.
    pub async fn push_audio(&self, a: AudioFrame) -> Result<(), PushError> {
        check_audio(&a)?;
        self.audio.send(a).await.map_err(|_| PushError::Closed)
    }

    /// Ends the input: ffmpeg flushes what it has (up to 3 s), then the
    /// stream ends.
    pub async fn finish(mut self) {
        // Swapping in senders of already-closed channels drops ours: the
        // end of input.
        self.video = mpsc::channel(1).0;
        self.audio = mpsc::channel(1).0;
        if let Some(mut task) = self.task.take()
            && tokio::time::timeout(DRAIN_TIMEOUT + Duration::from_secs(3), &mut task).await.is_err()
        {
            task.abort();
        }
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        if let Some(t) = &self.task {
            t.abort();
        }
    }
}

/// Aborts a task when dropped (so a dropped session takes its reader with
/// it, and the reader's hold on the publisher).
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

enum SessionEnd {
    /// The input ended.
    Input,
    /// ffmpeg died (or could not start).
    Crashed,
    /// A format changed; carry on with a fresh ffmpeg.
    Restart,
}

/// Input waiting for the next session.
#[derive(Default)]
struct Pending {
    video: Option<VideoFrame>,
    audio: Vec<AudioFrame>,
}

struct Inputs {
    video: mpsc::Receiver<VideoFrame>,
    audio: mpsc::Receiver<AudioFrame>,
    video_open: bool,
    audio_open: bool,
}

enum In {
    Video(VideoFrame),
    Audio(AudioFrame),
    Closed,
}

impl Inputs {
    async fn recv(&mut self) -> In {
        loop {
            if !self.video_open && !self.audio_open {
                return In::Closed;
            }
            tokio::select! {
                biased;
                a = self.audio.recv(), if self.audio_open => match a {
                    Some(a) => return In::Audio(a),
                    None => self.audio_open = false,
                },
                v = self.video.recv(), if self.video_open => match v {
                    Some(v) => return In::Video(v),
                    None => self.video_open = false,
                },
            }
        }
    }
}

async fn run(
    registry: Arc<Registry>,
    cfg: FeedConfig,
    video: mpsc::Receiver<VideoFrame>,
    audio: mpsc::Receiver<AudioFrame>,
) {
    let outs =
        Arc::new(Mutex::new(vec![RenditionOut::new(registry, cfg.stream.clone(), cfg.buffer, cfg.expect_audio)]));
    let mut inputs = Inputs { video, audio, video_open: true, audio_open: true };
    let mut pending = Pending::default();
    let mut backoff = MIN_BACKOFF;
    loop {
        let started = Instant::now();
        match session(&cfg, &outs, &mut inputs, &mut pending).await {
            SessionEnd::Input => return,
            SessionEnd::Restart => {
                backoff = MIN_BACKOFF;
                continue;
            }
            SessionEnd::Crashed => {}
        }
        if started.elapsed() >= HEALTHY_RUN {
            backoff = MIN_BACKOFF;
        }
        tracing::warn!(stream = %cfg.stream, retry_in = ?backoff, "ffmpeg exited; restarting");
        // Input arriving meanwhile is dropped; the restart takes the next.
        let sleep = tokio::time::sleep(backoff);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => break,
                i = inputs.recv() => if matches!(i, In::Closed) { return },
            }
        }
        pending = Pending::default();
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

fn args(cfg: &FeedConfig, v: &VideoFormat, audio: bool) -> Vec<String> {
    let fps = v.fps();
    let gop = (fps * 2.0).round().max(1.0) as u32;
    let k = cfg.video_kbps.max(1);
    let mut a: Vec<String> = Vec::new();
    let mut push = |s: &[&str]| a.extend(s.iter().map(|s| s.to_string()));
    push(&["-hide_banner", "-nostdin", "-nostats", "-loglevel", "info", "-copyts", "-fflags", "+nobuffer"]);
    // The Matroska header describes every stream completely: nothing to probe.
    push(&["-probesize", "32", "-analyzeduration", "0", "-f", "matroska", "-i", "pipe:0", "-map", "0:v:0"]);
    if audio {
        push(&["-map", "0:a:0"]);
    }
    // x264 takes NV12 and I420 as they are; UYVY goes through swscale
    // (measured cheap, see NOTES.md).
    if v.layout == PixelLayout::Uyvy {
        push(&["-vf", "format=yuv420p"]);
    }
    push(&[
        "-fps_mode",
        "passthrough",
        "-c:v",
        "libx264",
        "-preset",
        "veryfast",
        "-tune",
        "zerolatency",
        "-bf",
        "0",
        "-sc_threshold",
        "0",
        "-force_key_frames",
        "expr:if(isnan(prev_forced_t),1,gte(t-prev_forced_t,2))",
    ]);
    let gop = gop.to_string();
    let (b, buf) = (format!("{k}k"), format!("{}k", k * 2));
    push(&["-g", &gop, "-keyint_min", &gop, "-b:v", &b, "-maxrate", &b, "-bufsize", &buf]);
    if audio {
        let ab = format!("{}k", cfg.audio_kbps.max(8));
        push(&["-c:a", "aac", "-b:a", &ab]);
    }
    push(&["-f", "mpegts", "-mpegts_copyts", "1", "-muxdelay", "0", "-muxpreload", "0", "-flush_packets", "1"]);
    let start_pid = format!("0x{START_PID:x}");
    // Names the process for operators (and tests): `pgrep -f service_name=<stream>`.
    let service = format!("service_name={}", cfg.stream);
    push(&["-mpegts_start_pid", &start_pid, "-metadata", &service, "pipe:1"]);
    a
}

/// What one ffmpeg process was started for.
struct Formats {
    video: VideoFormat,
    audio: Option<(u32, u8)>,
}

/// Waits for the first video frame (and, if audio is expected, briefly for
/// audio). `None` if the input ended.
async fn wait_formats(cfg: &FeedConfig, inputs: &mut Inputs, pending: &mut Pending) -> Option<Formats> {
    let mut deadline: Option<tokio::time::Instant> = None;
    loop {
        if let Some(v) = &pending.video {
            let audio = pending.audio.last().map(AudioFrame::format);
            if audio.is_some() || !cfg.expect_audio || !inputs.audio_open {
                return Some(Formats { video: v.format, audio });
            }
            let d = *deadline.get_or_insert_with(|| tokio::time::Instant::now() + AUDIO_WAIT);
            match tokio::time::timeout_at(d, inputs.recv()).await {
                Err(_) => return Some(Formats { video: v.format, audio: None }),
                Ok(i) => take(i, pending)?,
            }
        } else {
            let i = inputs.recv().await;
            take(i, pending)?;
        }
    }
}

/// Stores input received before ffmpeg starts: the newest video frame and
/// up to 1 s of audio (only of the newest format).
fn take(i: In, pending: &mut Pending) -> Option<()> {
    match i {
        In::Closed => return None,
        In::Video(v) => pending.video = Some(v),
        In::Audio(a) => {
            if pending.audio.last().is_some_and(|l| l.format() != a.format()) {
                pending.audio.clear();
            }
            pending.audio.push(a);
            let rate = pending.audio[0].sample_rate as usize;
            let mut total: usize = pending.audio.iter().filter_map(AudioFrame::samples).sum();
            while total > rate && pending.audio.len() > 1 {
                total -= pending.audio.remove(0).samples().unwrap_or(0);
            }
        }
    }
    Some(())
}

/// Writes blocks into ffmpeg's stdin, keeping each track's pts increasing.
struct Writer {
    mkv: MkvWriter,
    clock: Clock,
    last_video: Option<i64>,
    /// End (pts + samples) of the last audio chunk written.
    audio_end: Option<i64>,
    header: Vec<u8>,
}

impl Writer {
    async fn video(&mut self, stdin: &mut ChildStdin, f: &VideoFrame) -> std::io::Result<()> {
        if self.last_video.is_some_and(|l| f.pts <= l) {
            tracing::debug!(pts = f.pts, "non-increasing video pts dropped");
            return Ok(());
        }
        self.last_video = Some(f.pts);
        let us = self.clock.onto_encoder_us(rescale(f.pts, 1_000_000, i64::from(VIDEO_HZ)));
        self.header.clear();
        self.mkv.block_header(VIDEO_IN, us, true, true, f.data.len(), &mut self.header);
        stdin.write_all(&self.header).await?;
        stdin.write_all(&f.data).await
    }

    async fn audio(&mut self, stdin: &mut ChildStdin, a: &AudioFrame) -> std::io::Result<()> {
        let Some(n) = a.samples() else { return Ok(()) };
        if self.audio_end.is_some_and(|e| a.pts < e) {
            tracing::debug!(pts = a.pts, "overlapping audio dropped");
            return Ok(());
        }
        self.audio_end = Some(a.pts + n as i64);
        let us = self.clock.onto_encoder_us(rescale(a.pts, 1_000_000, i64::from(a.sample_rate)));
        let data = a.interleaved();
        self.header.clear();
        self.mkv.block_header(AUDIO_IN, us, false, true, data.len(), &mut self.header);
        stdin.write_all(&self.header).await?;
        stdin.write_all(&data).await
    }
}

fn rescale(v: i64, to: i64, from: i64) -> i64 {
    (i128::from(v) * i128::from(to)).div_euclid(i128::from(from.max(1))) as i64
}

async fn session(
    cfg: &FeedConfig,
    outs: &Arc<Mutex<Vec<RenditionOut>>>,
    inputs: &mut Inputs,
    pending: &mut Pending,
) -> SessionEnd {
    let Some(formats) = wait_formats(cfg, inputs, pending).await else { return SessionEnd::Input };
    let fmt = formats.video;
    for o in outs.lock().expect("poisoned").iter_mut() {
        o.set_fps(fmt.fps());
    }
    let mut tracks =
        vec![MkvTrack::RawVideo { id: VIDEO_IN, width: fmt.width, height: fmt.height, fourcc: fmt.layout.fourcc() }];
    if let Some((sample_rate, channels)) = formats.audio {
        tracks.push(MkvTrack::PcmF32 { id: AUDIO_IN, sample_rate, channels });
    }
    let (mkv, header) = MkvWriter::with_tracks(&tracks, MKV_SCALE_NS);
    // The feed's timestamps already start near zero on a shared origin.
    let clock = Clock::fixed(0);
    let mut w = Writer { mkv, clock, last_video: None, audio_end: None, header: Vec::new() };

    let (proc, mut stdin, stdout) = match FfmpegProcess::spawn(
        &cfg.ffmpeg,
        &args(cfg, &fmt, formats.audio.is_some()),
        &cfg.stream,
    ) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(stream = %cfg.stream, ffmpeg = %cfg.ffmpeg.display(), error = %e, "cannot start ffmpeg");
            return SessionEnd::Crashed;
        }
    };
    tracing::debug!(stream = %cfg.stream, pid = ?proc.id(), "ffmpeg started");
    let per = if formats.audio.is_some() { 2 } else { 1 };
    let mut reader = AbortOnDrop(tokio::spawn(read_ts_outputs(stdout, outs.clone(), clock, per, 1)));

    let end = 'run: {
        if stdin.write_all(&header).await.is_err() {
            break 'run SessionEnd::Crashed;
        }
        // Audio first: it may start slightly before the picture.
        for a in std::mem::take(&mut pending.audio) {
            if Some(a.format()) == formats.audio && w.audio(&mut stdin, &a).await.is_err() {
                break 'run SessionEnd::Crashed;
            }
        }
        if let Some(v) = pending.video.take()
            && w.video(&mut stdin, &v).await.is_err()
        {
            break 'run SessionEnd::Crashed;
        }
        loop {
            let input = tokio::select! {
                i = inputs.recv() => i,
                _ = &mut reader.0 => break 'run SessionEnd::Crashed,
            };
            let res = match input {
                In::Closed => {
                    // Let ffmpeg flush what it has, then stop it.
                    drop(stdin);
                    let _ = tokio::time::timeout(DRAIN_TIMEOUT, &mut reader.0).await;
                    drop(reader);
                    proc.shutdown().await;
                    return SessionEnd::Input;
                }
                In::Video(v) if v.format != fmt => {
                    tracing::info!(stream = %cfg.stream, from = ?fmt, to = ?v.format, "video format changed; restarting ffmpeg");
                    pending.video = Some(v);
                    break 'run SessionEnd::Restart;
                }
                In::Audio(a) if Some(a.format()) != formats.audio => {
                    tracing::info!(stream = %cfg.stream, from = ?formats.audio, to = ?a.format(), "audio format changed; restarting ffmpeg");
                    pending.audio.push(a);
                    break 'run SessionEnd::Restart;
                }
                In::Video(v) => w.video(&mut stdin, &v).await,
                In::Audio(a) => w.audio(&mut stdin, &a).await,
            };
            if res.is_err() {
                break 'run SessionEnd::Crashed;
            }
        }
    };
    drop(reader);
    let tail = proc.shutdown().await;
    if matches!(end, SessionEnd::Crashed) {
        tracing::warn!(stream = %cfg.stream, stderr = ?tail, "ffmpeg stopped while the feed is live");
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_sizes() {
        assert_eq!(PixelLayout::Uyvy.frame_len(1920, 1080), Some(1920 * 1080 * 2));
        assert_eq!(PixelLayout::Nv12.frame_len(1920, 1080), Some(1920 * 1080 * 3 / 2));
        assert_eq!(PixelLayout::I420.frame_len(4, 2), Some(12));
        assert_eq!(PixelLayout::I420.frame_len(3, 2), None);
        assert_eq!(PixelLayout::Uyvy.frame_len(0, 2), None);
    }

    #[test]
    fn planar_audio_is_interleaved() {
        let samples: [f32; 6] = [1.0, 2.0, 3.0, 10.0, 20.0, 30.0]; // L L L R R R
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let a = AudioFrame {
            sample_rate: 48_000,
            channels: 2,
            layout: SampleLayout::Planar,
            pts: 0,
            data: Bytes::from(data),
        };
        assert_eq!(a.samples(), Some(3));
        let out: Vec<f32> = a.interleaved().as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
        assert_eq!(out, [1.0, 10.0, 2.0, 20.0, 3.0, 30.0]);
    }

    #[test]
    fn bad_frames_are_refused() {
        let format = VideoFormat { width: 4, height: 2, layout: PixelLayout::Uyvy, fps_num: 30, fps_den: 1 };
        assert_eq!(check_video(&VideoFrame { format, pts: 0, data: Bytes::from(vec![0; 16]) }), Ok(()));
        assert_eq!(
            check_video(&VideoFrame { format, pts: 0, data: Bytes::from(vec![0; 15]) }),
            Err(PushError::Invalid)
        );
        let a = AudioFrame {
            sample_rate: 48_000,
            channels: 2,
            layout: SampleLayout::Interleaved,
            pts: 0,
            data: Bytes::from(vec![0; 12]),
        };
        assert_eq!(check_audio(&a), Err(PushError::Invalid));
    }

    #[test]
    fn uyvy_gets_converted_and_audio_mapped() {
        let cfg = FeedConfig {
            ffmpeg: "ffmpeg".into(),
            stream: "cam".into(),
            buffer: BufferConfig::default(),
            video_kbps: 6000,
            audio_kbps: 128,
            expect_audio: true,
        };
        let v = VideoFormat { width: 1920, height: 1080, layout: PixelLayout::Uyvy, fps_num: 60000, fps_den: 1001 };
        let a = args(&cfg, &v, true).join(" ");
        assert!(a.contains("-map 0:v:0 -map 0:a:0 -vf format=yuv420p"), "{a}");
        assert!(a.contains("-g 120 "), "{a}");
        assert!(a.ends_with("service_name=cam pipe:1"), "{a}");
        let n = args(&cfg, &VideoFormat { layout: PixelLayout::Nv12, ..v }, false).join(" ");
        assert!(!n.contains("format=yuv420p") && !n.contains("0:a:0") && !n.contains("-c:a"), "{n}");
    }
}
