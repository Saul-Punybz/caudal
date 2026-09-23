//! Caudal streams out as OMT sources.
//!
//! Each [`OutputConfig`] turns one registry stream (a rendition such as
//! `main+720p` included) into an OMT sender announced as `MACHINE (name)`.
//!
//! - **Reading:** a tokio task per output waits for the stream (retrying
//!   while it is absent or ended), reads it with
//!   [`Stream::subscribe_internal`], and hands video and audio frames to the
//!   output's worker thread through a bounded channel. When the channel is
//!   full the video is dropped up to the next keyframe (counted in
//!   [`OutputStats::video_dropped`]), so a slow encoder never backs up the
//!   stream buffer.
//! - **Video:** H.264 only. 8-bit 4:2:0 Baseline/Main/High (CAVLC or
//!   CABAC, B-frames) decodes in process with `rusty_h264`, reordered to
//!   display order; other H.264 (High 10, 4:2:2, 4:4:4) goes through an
//!   ffmpeg subprocess (`-f h264` in, raw I420 out) when
//!   [`OutputOptions::ffmpeg`] is set. The I420 picture is VMX-encoded by
//!   [`Sender::send_video`]. Other codecs, or H.264 neither path can
//!   decode, are logged once and the output carries audio only.
//! - **Receivers:** nothing is decoded or encoded while no receiver is
//!   subscribed to video ([`Sender::video_receivers`]); the decoder then
//!   restarts at the next keyframe. Receivers count as the stream's viewers
//!   (`Stream::set_output_viewers("omt", n)`, summed over every OMT output
//!   of that stream).
//! - **Audio:** AAC-LC with `rusty_aac` and Opus (mono/stereo) with
//!   `opus-decoder`, in process; other AAC (HE-AAC) through an ffmpeg
//!   subprocess (ADTS in, `f32le` out) when ffmpeg is configured.
//!   [`OutputOptions::decode_with_ffmpeg`] sends everything through ffmpeg. Audio is decoded only
//!   while the sender has connections. Timestamps count samples from the
//!   first packet's pts, re-anchoring when the source jumps.
//! - **Threads:** decoding and encoding run on one std thread per output,
//!   which owns the [`Sender`], so the sender is created and dropped (its
//!   drop joins its threads, up to two seconds) off the tokio runtime.

mod audio;
mod video;

use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use caudal_core::{Codec, Event, Frame, Registry, StartAt, Stream, TrackInfo, TrackKind};
use caudal_transcode::pipe::FfmpegProcess;
use open_media_transport::command::{Quality, Tally};
use open_media_transport::discovery::Discovery;
use open_media_transport::sender::{Sender, SenderConfig, SenderInfo};

use self::audio::AudioOut;
use self::video::VideoOut;

/// Frames queued between an output's reader task and its worker thread
/// (about half a second of 30 fps video plus its audio).
const QUEUE: usize = 32;
/// How often the worker refreshes receiver counts and tally when idle.
const POLL: Duration = Duration::from_millis(250);
/// How long to wait between attempts to create a sender that failed.
const SENDER_RETRY: Duration = Duration::from_secs(5);

/// One `[[omt.output]]`: send `stream` out as the OMT source `name`.
#[derive(Clone)]
pub struct OutputConfig {
    /// Any registry stream, renditions (`main+720p`) included.
    pub stream: String,
    /// Source name; receivers see `MACHINE (name)`.
    pub name: String,
    /// VMX quality; `Default` follows the receivers' suggestions.
    pub quality: Quality,
    /// The most any receiver can make this output cost. OMT has no
    /// authentication: anyone who reaches the port can suggest `High`.
    /// With a cap set, suggestions are ignored and the sender encodes at
    /// `quality` (or at the cap when `quality` is `Default`), never above
    /// the cap. (The OMT library cannot clamp suggestions yet.)
    pub max_quality: Option<Quality>,
    /// VMX encoder threads (0 is taken as 1).
    pub encoder_threads: usize,
    /// Announce the source (DNS-SD, or through `discovery`). Receivers can
    /// always connect by `omt://host:port`.
    pub announce: bool,
    /// Announce through this shared discovery instead of one per sender.
    pub discovery: Option<Arc<Discovery>>,
}

impl OutputConfig {
    /// The quality the sender is configured with: `quality`, capped by
    /// `max_quality` (a cap turns `Default`, "follow the receivers", into
    /// the cap itself).
    pub fn effective_quality(&self) -> Quality {
        match (self.quality, self.max_quality) {
            (q, None) => q,
            (Quality::Default, Some(max)) => max,
            (q, Some(Quality::Default)) => q,
            (q, Some(max)) => q.min(max),
        }
    }

    /// An announced output with default quality and one encoder thread.
    pub fn new(stream: impl Into<String>, name: impl Into<String>) -> Self {
        OutputConfig {
            stream: stream.into(),
            name: name.into(),
            quality: Quality::Default,
            max_quality: None,
            encoder_threads: 1,
            announce: true,
            discovery: None,
        }
    }
}

impl PartialEq for OutputConfig {
    fn eq(&self, other: &Self) -> bool {
        self.stream == other.stream
            && self.name == other.name
            && self.quality == other.quality
            && self.max_quality == other.max_quality
            && self.encoder_threads == other.encoder_threads
            && self.announce == other.announce
            && match (&self.discovery, &other.discovery) {
                (None, None) => true,
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                _ => false,
            }
    }
}

impl fmt::Debug for OutputConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutputConfig")
            .field("stream", &self.stream)
            .field("name", &self.name)
            .field("quality", &self.quality)
            .field("max_quality", &self.max_quality)
            .field("encoder_threads", &self.encoder_threads)
            .field("announce", &self.announce)
            .field("shared_discovery", &self.discovery.is_some())
            .finish()
    }
}

/// Settings shared by every output.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutputOptions {
    /// ffmpeg binary, for what the in-process decoders cannot do: H.264
    /// other than 8-bit 4:2:0, and AAC other than AAC-LC. `None`: such
    /// tracks are skipped (or decoded band-limited, for HE-AAC) with a log
    /// line.
    pub ffmpeg: Option<PathBuf>,
    /// Decode video and AAC with ffmpeg even when the in-process decoders
    /// could (needs `ffmpeg`).
    pub decode_with_ffmpeg: bool,
}

/// Live counters of one output, shared with its worker. All relaxed atomics.
#[derive(Debug, Default)]
pub struct OutputStats {
    /// Video frames read from the stream.
    pub video_in: AtomicU64,
    /// Pictures VMX-encoded and handed to the sender.
    pub video_sent: AtomicU64,
    /// Video frames dropped because the output fell behind (queue full or
    /// stream buffer lag), each up to the next keyframe.
    pub video_dropped: AtomicU64,
    /// Video frames not decoded because no receiver wanted video, or while
    /// waiting for a keyframe to (re)start the decoder.
    pub video_skipped: AtomicU64,
    /// Audio frames sent to the sender.
    pub audio_sent: AtomicU64,
    /// Audio frames dropped because the output fell behind.
    pub audio_dropped: AtomicU64,
    /// Frames the decoders or the sender refused.
    pub errors: AtomicU64,
    /// Nanoseconds spent decoding, converting and VMX-encoding video.
    pub encode_nanos: AtomicU64,
    /// Receivers subscribed to video now.
    pub receivers: AtomicUsize,
    /// Open connections now (a typical receiver uses two).
    pub connections: AtomicUsize,
    /// Some receiver has this source on program.
    pub program: AtomicBool,
    /// Some receiver has this source on preview.
    pub preview: AtomicBool,
    /// The stream is live and being read.
    pub attached: AtomicBool,
    /// The sender's TCP port; 0 until the sender is up.
    pub port: AtomicU16,
    full_name: Mutex<Option<String>>,
}

impl OutputStats {
    /// The combined tally of the receivers, as last polled.
    pub fn tally(&self) -> Tally {
        Tally { program: self.program.load(Ordering::Relaxed), preview: self.preview.load(Ordering::Relaxed) }
    }

    /// `MACHINE (name)` once the sender announced itself.
    pub fn full_name(&self) -> Option<String> {
        self.full_name.lock().expect("poisoned").clone()
    }

    /// The sender's port, once it is up.
    pub fn port(&self) -> Option<u16> {
        Some(self.port.load(Ordering::Relaxed)).filter(|&p| p != 0)
    }

    fn add(counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
    }
}

/// One running output, for the API and metrics.
#[derive(Clone, Debug)]
pub struct OutputStatus {
    pub stream: String,
    pub name: String,
    pub stats: Arc<OutputStats>,
}

/// OMT receivers per stream, summed over every output of that stream.
#[derive(Default)]
struct ViewerBook {
    counts: Mutex<HashMap<String, Vec<(u64, usize)>>>,
}

impl ViewerBook {
    fn set(&self, stream: &Arc<Stream>, id: u64, n: usize) {
        let mut m = self.counts.lock().expect("poisoned");
        let v = m.entry(stream.name().to_owned()).or_default();
        match v.iter_mut().find(|(i, _)| *i == id) {
            Some(e) => e.1 = n,
            None => v.push((id, n)),
        }
        stream.set_output_viewers("omt", v.iter().map(|&(_, n)| n).sum());
    }

    fn remove(&self, stream: &Arc<Stream>, id: u64) {
        let mut m = self.counts.lock().expect("poisoned");
        let total = match m.get_mut(stream.name()) {
            Some(v) => {
                v.retain(|(i, _)| *i != id);
                v.iter().map(|&(_, n)| n).sum()
            }
            None => 0,
        };
        if total == 0 {
            m.remove(stream.name());
        }
        stream.set_output_viewers("omt", total);
    }
}

struct Entry {
    config: OutputConfig,
    stats: Arc<OutputStats>,
    task: tokio::task::JoinHandle<()>,
    worker: Option<std::thread::JoinHandle<()>>,
}

struct Inner {
    options: OutputOptions,
    entries: Vec<Entry>,
    book: Arc<ViewerBook>,
    next_id: u64,
    rt: tokio::runtime::Handle,
}

impl Inner {
    fn spawn(&mut self, registry: &Arc<Registry>, config: OutputConfig) -> Entry {
        let id = self.next_id;
        self.next_id += 1;
        let stats = Arc::new(OutputStats::default());
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        let worker = {
            let w = Worker {
                id,
                config: config.clone(),
                options: self.options.clone(),
                stats: stats.clone(),
                book: self.book.clone(),
                rt: self.rt.clone(),
            };
            std::thread::Builder::new().name(format!("omt-out:{}", config.name)).spawn(move || w.run(rx))
        };
        let worker = match worker {
            Ok(w) => Some(w),
            Err(e) => {
                tracing::error!(output = %config.name, error = %e, "cannot start the OMT output thread");
                None
            }
        };
        let task = {
            let (registry, stream, stats) = (registry.clone(), config.stream.clone(), stats.clone());
            self.rt.spawn(async move { read_stream(registry, stream, tx, stats).await })
        };
        Entry { config, stats, task, worker }
    }
}

/// A handle to the running `[[omt.output]]`s. Cheap to clone.
#[derive(Clone)]
pub struct OutputHandle {
    inner: Arc<Mutex<Inner>>,
}

/// Starts every configured output and returns immediately. Must be called
/// inside a tokio runtime. Each output is independent: its own task,
/// thread and sender.
pub fn start_outputs(registry: Arc<Registry>, options: OutputOptions, outputs: Vec<OutputConfig>) -> OutputHandle {
    let mut inner = Inner {
        options,
        entries: Vec::new(),
        book: Arc::default(),
        next_id: 0,
        rt: tokio::runtime::Handle::current(),
    };
    inner.entries = outputs.into_iter().map(|o| inner.spawn(&registry, o)).collect();
    OutputHandle { inner: Arc::new(Mutex::new(inner)) }
}

impl OutputHandle {
    /// Applies a new output list: an output whose config is unchanged (and
    /// options unchanged) keeps running untouched, with its receivers
    /// connected; removed outputs are stopped, changed or new ones
    /// (re)started. Stopped senders close within about two seconds.
    pub fn reload(&self, registry: &Arc<Registry>, options: OutputOptions, outputs: Vec<OutputConfig>) {
        let mut inner = self.inner.lock().expect("poisoned");
        let mut remaining = std::mem::take(&mut inner.entries);
        if inner.options != options {
            inner.options = options;
            for e in remaining.drain(..) {
                e.task.abort();
            }
        }
        let mut next = Vec::with_capacity(outputs.len());
        for o in outputs {
            if let Some(pos) = remaining.iter().position(|e| e.config == o) {
                next.push(remaining.remove(pos));
            } else {
                next.push(inner.spawn(registry, o));
            }
        }
        for gone in remaining {
            // Dropping the task drops the channel; the worker sees that,
            // drops its sender and exits on its own thread.
            gone.task.abort();
        }
        inner.entries = next;
    }

    /// Every running output with its counters.
    pub fn outputs(&self) -> Vec<OutputStatus> {
        let inner = self.inner.lock().expect("poisoned");
        inner
            .entries
            .iter()
            .map(|e| OutputStatus { stream: e.config.stream.clone(), name: e.config.name.clone(), stats: e.stats.clone() })
            .collect()
    }

    /// Stops every output without waiting; senders close within about two
    /// seconds, on their own threads.
    pub fn stop(&self) {
        let entries = std::mem::take(&mut self.inner.lock().expect("poisoned").entries);
        for e in entries {
            e.task.abort();
        }
    }

    /// Stops every output and waits (off the runtime) until every sender
    /// is closed.
    pub async fn shutdown(&self) {
        let entries = std::mem::take(&mut self.inner.lock().expect("poisoned").entries);
        let mut workers = Vec::new();
        for mut e in entries {
            e.task.abort();
            workers.extend(e.worker.take());
        }
        let _ = tokio::task::spawn_blocking(move || {
            for w in workers {
                let _ = w.join();
            }
        })
        .await;
    }
}

/// What the reader task hands the worker.
enum Item {
    /// The stream (re)appeared.
    Attach(Arc<Stream>),
    /// The selected tracks (initially, and whenever they change).
    Tracks { video: Option<TrackInfo>, audio: Option<TrackInfo> },
    Video(Arc<Frame>),
    Audio(Arc<Frame>),
    /// The stream ended.
    Detach,
}

/// The tracks an output uses: the first H.264 video track (or the first
/// video track, to be reported as unsupported) and the first AAC/Opus
/// audio track (or the first audio track).
fn select(tracks: &[TrackInfo]) -> (Option<TrackInfo>, Option<TrackInfo>) {
    let pick = |kind: TrackKind, preferred: &[Codec]| {
        tracks
            .iter()
            .find(|t| t.kind() == kind && preferred.contains(&t.codec))
            .or_else(|| tracks.iter().find(|t| t.kind() == kind))
            .cloned()
    };
    (pick(TrackKind::Video, &[Codec::H264]), pick(TrackKind::Audio, &[Codec::Aac, Codec::Opus]))
}

/// Sends a control item, waiting while the queue is full. `false` once the
/// worker is gone.
async fn send_control(tx: &SyncSender<Item>, mut item: Item) -> bool {
    loop {
        match tx.try_send(item) {
            Ok(()) => return true,
            Err(TrySendError::Full(i)) => {
                item = i;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Err(TrySendError::Disconnected(_)) => return false,
        }
    }
}

/// Waits for `name` to be live.
async fn wait_for_stream(registry: &Registry, name: &str) -> Arc<Stream> {
    let mut publishes = registry.subscribe_publishes();
    loop {
        if let Some(s) = registry.get(name).filter(|s| !s.is_ended()) {
            return s;
        }
        // Any publish (or a lag/close of that channel), or a periodic
        // re-check, whichever comes first.
        let _ = tokio::time::timeout(Duration::from_secs(1), publishes.recv()).await;
    }
}

/// The reader task: stream → worker, forever (until aborted or the worker
/// is gone).
async fn read_stream(registry: Arc<Registry>, name: String, tx: SyncSender<Item>, stats: Arc<OutputStats>) {
    loop {
        let stream = wait_for_stream(&registry, &name).await;
        let mut sub = stream.subscribe_internal(StartAt::LiveEdge);
        if !send_control(&tx, Item::Attach(stream)).await {
            return;
        }
        stats.attached.store(true, Ordering::Relaxed);
        let (mut video_id, mut audio_id) = (None, None);
        let mut resync = true;
        loop {
            let item = match sub.recv().await {
                Event::End => break,
                Event::Cue(_) => continue,
                Event::Lagged { skipped } => {
                    OutputStats::add(&stats.video_dropped, skipped);
                    resync = true;
                    continue;
                }
                Event::TracksChanged => {
                    let (video, audio) = select(&sub.tracks());
                    video_id = video.as_ref().map(|t| t.id);
                    audio_id = audio.as_ref().map(|t| t.id);
                    Item::Tracks { video, audio }
                }
                Event::Frame(f) if Some(f.track) == video_id => {
                    if resync && !f.keyframe {
                        OutputStats::add(&stats.video_dropped, 1);
                        continue;
                    }
                    resync = false;
                    Item::Video(f)
                }
                Event::Frame(f) if Some(f.track) == audio_id => Item::Audio(f),
                Event::Frame(_) => continue,
            };
            let control = matches!(item, Item::Tracks { .. });
            if control {
                if !send_control(&tx, item).await {
                    return;
                }
                continue;
            }
            match tx.try_send(item) {
                Ok(()) => {}
                Err(TrySendError::Full(Item::Video(_))) => {
                    tracing::debug!(stream = %name, "OMT output is behind; skipping to the next keyframe");
                    OutputStats::add(&stats.video_dropped, 1);
                    resync = true;
                }
                Err(TrySendError::Full(_)) => OutputStats::add(&stats.audio_dropped, 1),
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
        stats.attached.store(false, Ordering::Relaxed);
        if !send_control(&tx, Item::Detach).await {
            return;
        }
    }
}

/// What the video and audio paths share.
pub(crate) struct Ctx {
    pub sender: Arc<Sender>,
    pub stats: Arc<OutputStats>,
    pub options: OutputOptions,
    pub rt: tokio::runtime::Handle,
    /// The output's name, for logs.
    pub name: String,
}

impl Ctx {
    /// Starts ffmpeg with `args`, returning its stdin and stdout as blocking
    /// files for this output's threads.
    pub fn spawn_ffmpeg(&self, args: &[String]) -> std::io::Result<(FfmpegProcess, File, File)> {
        let Some(ffmpeg) = &self.options.ffmpeg else {
            return Err(std::io::Error::other("no ffmpeg configured"));
        };
        let _rt = self.rt.enter();
        let (proc, stdin, stdout) = FfmpegProcess::spawn(ffmpeg, args, &self.name)?;
        Ok((proc, File::from(stdin.into_owned_fd()?), File::from(stdout.into_owned_fd()?)))
    }
}

struct Worker {
    id: u64,
    config: OutputConfig,
    options: OutputOptions,
    stats: Arc<OutputStats>,
    book: Arc<ViewerBook>,
    rt: tokio::runtime::Handle,
}

impl Worker {
    /// Everything the sender is built from comes from here, so the access
    /// controls OMT lacks can be passed through in one place.
    // TODO(omt): pass a bind address, a connection limit and a peer
    // allow-list through when the OMT library's next version exposes them
    // (OMT has no authentication; today any host that reaches the port can
    // connect, set tally and suggest quality).
    fn sender_config(&self) -> SenderConfig {
        let c = &self.config;
        SenderConfig {
            quality: c.effective_quality(),
            info: Some(SenderInfo {
                product_name: "Caudal".into(),
                manufacturer: "Caudal".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            }),
            announce: c.announce,
            discovery: c.discovery.clone(),
            encoder_threads: c.encoder_threads.max(1),
            ..SenderConfig::new(c.name.clone())
        }
    }

    /// Creates the sender, retrying while it fails. `None` if the output
    /// was stopped meanwhile.
    fn open_sender(&self, rx: &mpsc::Receiver<Item>) -> Option<Sender> {
        loop {
            match Sender::new(self.sender_config()) {
                Ok(s) => return Some(s),
                Err(e) => {
                    tracing::error!(output = %self.config.name, error = %e, "cannot start the OMT sender; retrying");
                    let deadline = Instant::now() + SENDER_RETRY;
                    // Drain (and drop) frames meanwhile; stop if the output is gone.
                    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
                        match rx.recv_timeout(left) {
                            Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                            Err(RecvTimeoutError::Disconnected) => return None,
                        }
                    }
                }
            }
        }
    }

    fn run(self, rx: mpsc::Receiver<Item>) {
        let Some(sender) = self.open_sender(&rx) else { return };
        let sender = Arc::new(sender);
        self.stats.port.store(sender.port(), Ordering::Relaxed);
        *self.stats.full_name.lock().expect("poisoned") = sender.full_name().map(str::to_owned);
        tracing::info!(output = %self.config.name, stream = %self.config.stream, port = sender.port(),
            name = sender.full_name().unwrap_or("(not announced)"), "OMT output started");
        let ctx = Ctx {
            sender,
            stats: self.stats.clone(),
            options: self.options.clone(),
            rt: self.rt.clone(),
            name: self.config.name.clone(),
        };
        let mut stream: Option<Arc<Stream>> = None;
        let mut video: Option<VideoOut> = None;
        let mut audio: Option<AudioOut> = None;
        let mut last_poll = Instant::now() - POLL;
        loop {
            match rx.recv_timeout(POLL) {
                Ok(Item::Attach(s)) => {
                    (video, audio) = (None, None);
                    if let Some(old) = stream.replace(s) {
                        self.book.remove(&old, self.id);
                    }
                    last_poll = Instant::now() - POLL;
                }
                Ok(Item::Tracks { video: v, audio: a }) => {
                    if video.as_ref().map(VideoOut::track) != v.as_ref() {
                        video = v.map(|t| VideoOut::new(t, &ctx));
                    }
                    if audio.as_ref().map(AudioOut::track) != a.as_ref() {
                        audio = a.map(|t| AudioOut::new(t, &ctx));
                    }
                }
                Ok(Item::Video(f)) => {
                    if let Some(v) = &mut video {
                        v.push(&f, &ctx);
                    }
                }
                Ok(Item::Audio(f)) => {
                    if let Some(a) = &mut audio {
                        a.push(&f, &ctx);
                    }
                }
                Ok(Item::Detach) => {
                    (video, audio) = (None, None);
                    if let Some(old) = stream.take() {
                        self.book.remove(&old, self.id);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if last_poll.elapsed() >= POLL {
                last_poll = Instant::now();
                let receivers = ctx.sender.video_receivers();
                let tally = ctx.sender.tally();
                self.stats.receivers.store(receivers, Ordering::Relaxed);
                self.stats.connections.store(ctx.sender.connections(), Ordering::Relaxed);
                self.stats.program.store(tally.program, Ordering::Relaxed);
                self.stats.preview.store(tally.preview, Ordering::Relaxed);
                if let Some(s) = &stream {
                    self.book.set(s, self.id, receivers);
                }
            }
        }
        // Decoders first (they may hold the sender in their own threads),
        // then the sender, all on this thread.
        drop((video, audio));
        if let Some(s) = stream {
            self.book.remove(&s, self.id);
        }
        self.stats.attached.store(false, Ordering::Relaxed);
        self.stats.receivers.store(0, Ordering::Relaxed);
        self.stats.connections.store(0, Ordering::Relaxed);
        drop(ctx);
        tracing::info!(output = %self.config.name, "OMT output stopped");
    }
}

/// `fps` as the rational OMT headers carry: NTSC rates as `n/1001`,
/// others in thousandths, reduced.
pub(crate) fn frame_rate(fps: f64) -> (i32, i32) {
    if !(fps.is_finite() && fps > 0.0 && fps <= 1000.0) {
        return (30, 1);
    }
    let ntsc = fps * 1.001;
    if (ntsc - ntsc.round()).abs() < 0.002 && (fps - fps.round()).abs() > 0.002 {
        return ((ntsc.round() as i32) * 1000, 1001);
    }
    let (mut n, mut d) = ((fps * 1000.0).round() as i32, 1000);
    let (mut a, mut b) = (n, d);
    while b != 0 {
        (a, b) = (b, a % b);
    }
    if a > 1 {
        n /= a;
        d /= a;
    }
    (n, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_rates() {
        assert_eq!(frame_rate(30.0), (30, 1));
        assert_eq!(frame_rate(29.97), (30000, 1001));
        assert_eq!(frame_rate(30000.0 / 1001.0), (30000, 1001));
        assert_eq!(frame_rate(59.94), (60000, 1001));
        assert_eq!(frame_rate(23.976), (24000, 1001));
        assert_eq!(frame_rate(25.0), (25, 1));
        assert_eq!(frame_rate(12.5), (25, 2));
        assert_eq!(frame_rate(f64::NAN), (30, 1));
        assert_eq!(frame_rate(0.0), (30, 1));
    }

    #[test]
    fn config_equality_compares_discovery_by_identity() {
        let a = OutputConfig::new("main", "Main");
        assert_eq!(a, a.clone());
        let b = OutputConfig { quality: Quality::High, ..a.clone() };
        assert_ne!(a, b);
        assert_ne!(a, OutputConfig { max_quality: Some(Quality::Low), ..a.clone() });
    }

    #[test]
    fn quality_cap() {
        let q = |quality, max_quality| {
            OutputConfig { quality, max_quality, ..OutputConfig::new("s", "n") }.effective_quality()
        };
        assert_eq!(q(Quality::Default, None), Quality::Default);
        assert_eq!(q(Quality::High, None), Quality::High);
        // A cap stops following suggestions.
        assert_eq!(q(Quality::Default, Some(Quality::Medium)), Quality::Medium);
        assert_eq!(q(Quality::High, Some(Quality::Medium)), Quality::Medium);
        assert_eq!(q(Quality::Low, Some(Quality::High)), Quality::Low);
        assert_eq!(q(Quality::High, Some(Quality::Default)), Quality::High);
    }
}
