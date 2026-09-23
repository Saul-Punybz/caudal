//! OMT pull ingest: each [`PullConfig`] connects to an OMT source (a full
//! name `MACHINE (Name)` or `omt://host:port`), decodes VMX video to 8-bit
//! UYVY and FPA1 audio to planar f32 in Rust, and hands both to a
//! [`Feed`] (one ffmpeg: H.264 + AAC) that publishes them as `stream`.
//!
//! **Threads.** One `std::thread` per pull (`omt-pull-<stream>`): the OMT
//! [`Receiver`] is blocking, VMX decoding is CPU work, and both the
//! receiver's and a decoder's teardown join threads, so none of it runs on
//! the async runtime. The thread polls the receiver with a 100 ms timeout and
//! checks its stop flag between polls; the [`Feed`] is started from it
//! through the runtime handle captured by [`start_pulls`]. Nothing is
//! dropped on the runtime: the thread owns the receiver, the decoder and the
//! feed, and drops them itself when it stops.
//!
//! **Falling behind.** The receiver queues every frame it reads. Each poll
//! drains what is already queued (up to [`MAX_BATCH`] events), pushes every
//! audio chunk, and decodes only the newest picture; older pictures are
//! dropped (`DropReason::Behind`). VMX is intra-only, so any picture can go.
//! A full feed queue (ffmpeg behind) drops the frame too (`QueueFull`);
//! nothing ever blocks the receive loop.
//!
//! **Formats.** Video is always decoded to 8-bit UYVY
//! (`PreferredVideoFormat::Uyvy`): a 10-bit source goes through VMX's
//! 8-bit path (as libomtnet does for that preference) and alpha is
//! discarded. A 10-bit H.264/HEVC path is future work (see `NOTES.md`).
//! A size or frame-rate change reaches the feed as a new [`VideoFormat`],
//! and the feed restarts ffmpeg while the stream stays published. Audio
//! keeps at most 8 channels ([`crate::audio`]).
//!
//! **Connecting and outages.** The first connection is retried with backoff
//! (1 s doubling to 30 s) until it succeeds; after that the receiver itself
//! reconnects (once a second, re-resolving the name or URL each time). The
//! stream is started at the first media frame. Unlike the RTSP pull, which
//! ends its stream on every disconnect, a short outage keeps the stream
//! published: the feed (and its ffmpeg) stay up, viewers just see no new
//! frames, and when the source returns [`TimeMap`] re-anchors its clock so
//! timestamps continue past the gap. After [`OUTAGE_GRACE`] without media
//! the feed is dropped, which ends the stream (players and recorders see a
//! clean end); the next media frame publishes it again. If another publisher
//! holds the stream name when a feed would start, frames are dropped
//! (`DropReason::Busy`) until the name is free.
//!
//! **Tally.** Sent to the source at most once a second, when it changes:
//! program while the Caudal stream has viewers ([`StreamStats::viewers`],
//! which includes outputs' own counts such as HLS), preview while it is
//! published by this pull. Caudal cannot count internal subscribers
//! (packagers, recorders) through `caudal-core` today, so "preview" means
//! "ingested", not "consumed internally".
//!
//! [`StreamStats::viewers`]: caudal_core::StreamStats

use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bytes::Bytes;
use caudal_core::{BufferConfig, Registry};
use open_media_transport::OwnedFrame;
use open_media_transport::address::{Address, Directory};
use open_media_transport::command::{Command, Quality, Tally};
use open_media_transport::frame::ExtendedHeader;
use open_media_transport::media::{self, MediaDecoder, PreferredVideoFormat};
use open_media_transport::receiver::{Event, Receiver, ReceiverConfig};

use crate::feed::{Feed, FeedConfig, PixelLayout, PushError, VideoFormat, VideoFrame};
use crate::time::{OMT_HZ, TimeMap};

/// How long the receive loop waits for an event before checking its stop
/// flag and tally.
const POLL: Duration = Duration::from_millis(100);
/// Most events handled per poll before the newest picture is decoded.
pub const MAX_BATCH: usize = 64;
/// A stream is kept published this long without media from the source.
pub const OUTAGE_GRACE: Duration = Duration::from_secs(5);
/// Minimum time between tally updates.
const TALLY_INTERVAL: Duration = Duration::from_secs(1);
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Frame rate assumed when the sender's is missing or invalid.
const FALLBACK_FPS: (u32, u32) = (30, 1);

/// One `[[omt.pull]]`: pull the OMT source `source` into Caudal as `stream`.
#[derive(Clone)]
pub struct PullConfig {
    /// Caudal stream name the H.264/AAC result is published under.
    pub stream: String,
    /// OMT address: a full name `MACHINE (Name)` (looked up in `directory`,
    /// or a directory the receiver starts for itself) or `omt://host:port`.
    pub source: String,
    /// Encoder quality suggested to the sender.
    pub quality: Quality,
    /// H.264 bitrate.
    pub video_kbps: u32,
    /// AAC bitrate.
    pub audio_kbps: u32,
    /// ffmpeg binary.
    pub ffmpeg: PathBuf,
    /// Shared source directory (mDNS / discovery server) for resolving full
    /// names; `None` lets each receiver start its own when it needs one.
    pub directory: Option<Arc<Directory>>,
}

impl PartialEq for PullConfig {
    /// Every field equal; directories compared by identity.
    fn eq(&self, o: &Self) -> bool {
        self.stream == o.stream
            && self.source == o.source
            && self.quality == o.quality
            && self.video_kbps == o.video_kbps
            && self.audio_kbps == o.audio_kbps
            && self.ffmpeg == o.ffmpeg
            && match (&self.directory, &o.directory) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            }
    }
}

impl fmt::Debug for PullConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PullConfig")
            .field("stream", &self.stream)
            .field("source", &self.source)
            .field("quality", &self.quality)
            .field("video_kbps", &self.video_kbps)
            .field("audio_kbps", &self.audio_kbps)
            .field("ffmpeg", &self.ffmpeg)
            .field("directory", &self.directory.is_some())
            .finish()
    }
}

/// Why a received frame did not reach the encoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropReason {
    /// A newer picture was already queued by the receiver (the pull thread
    /// was behind); only the newest is decoded.
    Behind,
    /// The feed's queue was full (ffmpeg behind).
    QueueFull,
    /// VMX/FPA1 decoding failed, or an unsupported codec.
    Decode,
    /// Decoded, but with a size, rate or layout the feed refuses (odd
    /// width/height, no samples, ...).
    Invalid,
    /// Audio whose sample count ran ahead of the sender's timestamps
    /// ([`TimeMap::audio`] returned `None`).
    Timing,
    /// Another publisher holds the stream name.
    Busy,
}

impl DropReason {
    /// Every reason, in [`DropReason::index`] order.
    pub const ALL: [DropReason; 6] = [
        DropReason::Behind,
        DropReason::QueueFull,
        DropReason::Decode,
        DropReason::Invalid,
        DropReason::Timing,
        DropReason::Busy,
    ];

    /// Label for metrics (`reason="..."`).
    pub fn as_str(self) -> &'static str {
        match self {
            DropReason::Behind => "behind",
            DropReason::QueueFull => "queue_full",
            DropReason::Decode => "decode",
            DropReason::Invalid => "invalid",
            DropReason::Timing => "timing",
            DropReason::Busy => "busy",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// Live counters of one pull, shared with the pull thread. All counters are
/// totals since the pull started (a pull restarted by a reload gets a new
/// `PullStats`). Read them with [`PullStats::snapshot`] or the atomics.
#[derive(Debug, Default)]
pub struct PullStats {
    /// Whether every OMT connection (video + audio) is up right now.
    pub connected: AtomicBool,
    /// Video frames received from the source (before any drop).
    pub video_in: AtomicU64,
    /// Audio frames received from the source (before any drop).
    pub audio_in: AtomicU64,
    /// Pictures queued into the encoder.
    pub video_pushed: AtomicU64,
    /// Audio chunks queued into the encoder.
    pub audio_pushed: AtomicU64,
    /// Frames dropped, by [`DropReason`] (index = position in
    /// [`DropReason::ALL`]); see [`PullStats::dropped`].
    pub dropped: [AtomicU64; 6],
    /// Times the connection to the source was re-established after it was
    /// lost (the receiver's own count; the first connection is not one).
    pub reconnects: AtomicU64,
    /// Bytes read from the source, both connections.
    pub bytes_in: AtomicU64,
    /// Times the stream was (re)published, i.e. a feed was started: 1 for
    /// a source that never went away longer than [`OUTAGE_GRACE`].
    pub publishes: AtomicU64,
    /// Tally last sent to the source: bit 0 preview, bit 1 program.
    pub tally: AtomicU8,
}

/// A plain copy of [`PullStats`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PullStatsSnapshot {
    pub connected: bool,
    pub video_in: u64,
    pub audio_in: u64,
    pub video_pushed: u64,
    pub audio_pushed: u64,
    /// Indexed like [`DropReason::ALL`].
    pub dropped: [u64; 6],
    pub reconnects: u64,
    pub bytes_in: u64,
    pub publishes: u64,
    pub tally: Tally,
}

impl PullStats {
    /// Frames dropped for `reason`.
    pub fn dropped(&self, reason: DropReason) -> u64 {
        self.dropped[reason.index()].load(Ordering::Relaxed)
    }

    /// Tally last sent to the source.
    pub fn tally(&self) -> Tally {
        let t = self.tally.load(Ordering::Relaxed);
        Tally { preview: t & 1 != 0, program: t & 2 != 0 }
    }

    pub fn snapshot(&self) -> PullStatsSnapshot {
        let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
        PullStatsSnapshot {
            connected: self.connected.load(Ordering::Relaxed),
            video_in: l(&self.video_in),
            audio_in: l(&self.audio_in),
            video_pushed: l(&self.video_pushed),
            audio_pushed: l(&self.audio_pushed),
            dropped: std::array::from_fn(|i| l(&self.dropped[i])),
            reconnects: l(&self.reconnects),
            bytes_in: l(&self.bytes_in),
            publishes: l(&self.publishes),
            tally: self.tally(),
        }
    }

    fn drop_frame(&self, reason: DropReason) {
        self.dropped[reason.index()].fetch_add(1, Ordering::Relaxed);
    }

    fn inc(a: &AtomicU64) {
        a.fetch_add(1, Ordering::Relaxed);
    }
}

/// One running pull, as [`PullHandle::statuses`] reports it.
#[derive(Debug, Clone)]
pub struct PullStatus {
    pub stream: String,
    pub source: String,
    pub stats: Arc<PullStats>,
}

struct PullEntry {
    pull: PullConfig,
    stop: Arc<AtomicBool>,
    stats: Arc<PullStats>,
    thread: Option<JoinHandle<()>>,
}

impl PullEntry {
    fn signal(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

struct Inner {
    entries: Mutex<Vec<PullEntry>>,
    rt: tokio::runtime::Handle,
}

impl Drop for Inner {
    fn drop(&mut self) {
        for e in self.entries.get_mut().unwrap_or_else(|p| p.into_inner()).iter() {
            e.signal();
        }
    }
}

/// A handle to the running `[[omt.pull]]` threads. Cheap to clone; when the
/// last clone is dropped every pull is told to stop (their streams end).
#[derive(Clone)]
pub struct PullHandle {
    inner: Arc<Inner>,
}

fn spawn_pull(
    rt: &tokio::runtime::Handle,
    registry: Arc<Registry>,
    buffer: BufferConfig,
    pull: PullConfig,
) -> PullEntry {
    let stop = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(PullStats::default());
    let ctx = Ctx { pull: pull.clone(), registry, buffer, rt: rt.clone(), stop: stop.clone(), stats: stats.clone() };
    let thread = std::thread::Builder::new().name(format!("omt-pull-{}", pull.stream)).spawn(move || run(ctx));
    let thread = match thread {
        Ok(t) => Some(t),
        Err(e) => {
            tracing::error!(stream = %pull.stream, error = %e, "omt pull: cannot start thread");
            None
        }
    };
    PullEntry { pull, stop, stats, thread }
}

/// Starts every configured OMT pull, each on its own thread, and returns
/// immediately. Must be called inside a tokio runtime (the feeds run on it).
pub fn start_pulls(registry: Arc<Registry>, buffer: BufferConfig, pulls: Vec<PullConfig>) -> PullHandle {
    let rt = tokio::runtime::Handle::current();
    let entries = pulls.into_iter().map(|p| spawn_pull(&rt, registry.clone(), buffer, p)).collect();
    PullHandle { inner: Arc::new(Inner { entries: Mutex::new(entries), rt }) }
}

impl PullHandle {
    /// Applies a new pull list: a pull whose [`PullConfig`] is unchanged
    /// keeps its thread, connection and stream untouched; removed pulls are
    /// stopped (their streams end), changed or new ones (re)started. A
    /// restarted pull's stream ends and is published again by the new one.
    pub fn reload(&self, registry: &Arc<Registry>, buffer: BufferConfig, pulls: Vec<PullConfig>) {
        let mut entries = self.inner.entries.lock().unwrap_or_else(|p| p.into_inner());
        let mut remaining = std::mem::take(&mut *entries);
        let mut next = Vec::with_capacity(pulls.len());
        for p in pulls {
            if let Some(pos) = remaining.iter().position(|e| e.pull == p) {
                next.push(remaining.remove(pos));
            } else {
                next.push(spawn_pull(&self.inner.rt, registry.clone(), buffer, p));
            }
        }
        for gone in remaining {
            tracing::info!(stream = %gone.pull.stream, "omt pull: stopping");
            gone.signal();
        }
        *entries = next;
    }

    /// Tells every pull to stop, without waiting; their streams end within
    /// about 100 ms (a pull still connecting stops when that attempt ends).
    pub fn stop(&self) {
        let entries = std::mem::take(&mut *self.inner.entries.lock().unwrap_or_else(|p| p.into_inner()));
        for e in &entries {
            e.signal();
        }
    }

    /// [`PullHandle::stop`], then waits (off the runtime) for the threads to
    /// finish.
    pub async fn shutdown(&self) {
        let entries = std::mem::take(&mut *self.inner.entries.lock().unwrap_or_else(|p| p.into_inner()));
        for e in &entries {
            e.signal();
        }
        let threads: Vec<JoinHandle<()>> = entries.into_iter().filter_map(|mut e| e.thread.take()).collect();
        let _ = tokio::task::spawn_blocking(move || {
            for t in threads {
                let _ = t.join();
            }
        })
        .await;
    }

    /// The configured pulls with their live counters.
    pub fn statuses(&self) -> Vec<PullStatus> {
        let entries = self.inner.entries.lock().unwrap_or_else(|p| p.into_inner());
        entries
            .iter()
            .map(|e| PullStatus {
                stream: e.pull.stream.clone(),
                source: e.pull.source.clone(),
                stats: e.stats.clone(),
            })
            .collect()
    }
}

struct Ctx {
    pull: PullConfig,
    registry: Arc<Registry>,
    buffer: BufferConfig,
    rt: tokio::runtime::Handle,
    stop: Arc<AtomicBool>,
    stats: Arc<PullStats>,
}

impl Ctx {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// Sleeps `d` in poll-sized steps; `false` if stopped meanwhile.
    fn sleep(&self, d: Duration) -> bool {
        let end = Instant::now() + d;
        while !self.stopped() {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return true;
            }
            std::thread::sleep(left.min(POLL));
        }
        false
    }
}

fn run(ctx: Ctx) {
    let p = &ctx.pull;
    if !caudal_core::media::valid_stream_name(&p.stream) {
        tracing::error!(stream = %p.stream, "omt pull: invalid stream name; not started");
        return;
    }
    let address = match Address::parse(&p.source) {
        Ok(a) => a,
        Err(e) => {
            tracing::error!(stream = %p.stream, source = %p.source, error = %e, "omt pull: invalid source; not started");
            return;
        }
    };
    let config = ReceiverConfig {
        video: true,
        audio: true,
        preview: false,
        quality: p.quality,
        tally: Tally::default(),
        reconnect: true,
        follow_redirects: true,
    };
    let mut backoff = MIN_BACKOFF;
    let rx = loop {
        if ctx.stopped() {
            return;
        }
        tracing::info!(stream = %p.stream, source = %p.source, "omt pull: connecting");
        match Receiver::connect_address(address.clone(), config, p.directory.clone()) {
            Ok(rx) => break rx,
            Err(e) => {
                tracing::warn!(stream = %p.stream, source = %p.source, error = %e, retry_in = ?backoff, "omt pull failed");
                if !ctx.sleep(backoff) {
                    return;
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    };
    tracing::info!(stream = %p.stream, source = %p.source, peer = ?rx.peer_addr(), "omt pull: connected");
    let mut ingest = Ingest::new(&ctx);
    ingest.receive(&rx);
    // Stream first (ends at once), then the receiver (its drop joins its
    // threads, bounded).
    drop(ingest);
    drop(rx);
    tracing::info!(stream = %p.stream, "omt pull: stopped");
}

/// Program while viewed, preview while published (see the module docs).
fn tally_for(registry: &Registry, stream: &str) -> Tally {
    match registry.get(stream) {
        Some(s) if !s.is_ended() => Tally { preview: true, program: s.stats().viewers > 0 },
        _ => Tally::default(),
    }
}

fn tally_bits(t: Tally) -> u8 {
    u8::from(t.preview) | (u8::from(t.program) << 1)
}

/// `(num, den)` of a sender's frame rate, or [`FALLBACK_FPS`].
fn frame_rate(n: i32, d: i32) -> (u32, u32) {
    match (u32::try_from(n), u32::try_from(d)) {
        (Ok(n), Ok(d)) if n > 0 && d > 0 && f64::from(n) / f64::from(d) <= 240.0 => (n, d),
        _ => FALLBACK_FPS,
    }
}

/// Decoding and feeding state of one pull.
struct Ingest<'a> {
    ctx: &'a Ctx,
    decoder: MediaDecoder,
    video: media::VideoFrame,
    audio: media::AudioFrame,
    map: TimeMap,
    feed: Option<Feed>,
    /// When the last media frame arrived from the source.
    last_media: Instant,
    /// A busy stream name was already reported (until the next publish).
    busy_logged: bool,
}

impl<'a> Ingest<'a> {
    fn new(ctx: &'a Ctx) -> Self {
        Ingest {
            ctx,
            decoder: MediaDecoder::new(PreferredVideoFormat::Uyvy),
            video: media::VideoFrame::default(),
            audio: media::AudioFrame::default(),
            map: TimeMap::new(),
            feed: None,
            last_media: Instant::now(),
            busy_logged: false,
        }
    }

    fn receive(&mut self, rx: &Receiver) {
        let (ctx, stats) = (self.ctx, &*self.ctx.stats);
        let stream = ctx.pull.stream.as_str();
        let mut tally = Tally::default();
        let mut tally_at: Option<Instant> = None;
        while !ctx.stopped() {
            if tally_at.is_none_or(|t| t.elapsed() >= TALLY_INTERVAL) {
                tally_at = Some(Instant::now());
                let t = tally_for(&ctx.registry, stream);
                if t != tally {
                    // Remembered by the receiver and re-sent on reconnect,
                    // even if this send fails.
                    let _ = rx.send(Command::Tally(t));
                    tracing::debug!(stream, tally = ?t, "omt pull: tally");
                    tally = t;
                    stats.tally.store(tally_bits(t), Ordering::Relaxed);
                }
            }

            let mut newest: Option<OwnedFrame> = None;
            let mut wait = POLL;
            for _ in 0..MAX_BATCH {
                let Some(ev) = rx.recv_timeout(wait) else { break };
                wait = Duration::ZERO;
                match ev {
                    Event::Frame(_, f) => match f.ext {
                        ExtendedHeader::Video(_) => {
                            PullStats::inc(&stats.video_in);
                            self.last_media = Instant::now();
                            if newest.replace(f).is_some() {
                                stats.drop_frame(DropReason::Behind);
                            }
                        }
                        ExtendedHeader::Audio(_) => {
                            PullStats::inc(&stats.audio_in);
                            self.last_media = Instant::now();
                            self.on_audio(&f);
                        }
                        ExtendedHeader::None => {}
                    },
                    Event::Connected(channel) => {
                        tracing::info!(stream, ?channel, peer = ?rx.peer_addr(), "omt pull: channel connected");
                    }
                    Event::Closed(channel, reason) => {
                        tracing::warn!(stream, ?channel, ?reason, "omt pull: source disconnected; reconnecting");
                    }
                    Event::Redirect(to) => {
                        tracing::info!(stream, redirect = ?to, "omt pull: source redirected");
                    }
                }
            }
            if let Some(f) = newest {
                self.on_video(&f);
            }

            let s = rx.stats();
            stats.connected.store(rx.is_connected(), Ordering::Relaxed);
            stats.reconnects.store(s.reconnects, Ordering::Relaxed);
            stats.bytes_in.store(s.video.bytes + s.audio.bytes, Ordering::Relaxed);

            if self.feed.is_some() && self.last_media.elapsed() > OUTAGE_GRACE {
                tracing::info!(stream, grace = ?OUTAGE_GRACE, "omt pull: no media from the source; ending the stream");
                self.feed = None;
            }
        }
    }

    /// Starts the feed if there is none. `false` when the name is taken (the
    /// frame is dropped) or the feed cannot start.
    fn ensure_feed(&mut self) -> bool {
        if self.feed.is_some() {
            return true;
        }
        let ctx = self.ctx;
        let p = &ctx.pull;
        if ctx.registry.get(&p.stream).is_some() {
            if !self.busy_logged {
                tracing::warn!(stream = %p.stream, "omt pull: stream name busy; dropping frames until it is free");
                self.busy_logged = true;
            }
            ctx.stats.drop_frame(DropReason::Busy);
            return false;
        }
        let cfg = FeedConfig {
            ffmpeg: p.ffmpeg.clone(),
            stream: p.stream.clone(),
            buffer: ctx.buffer,
            video_kbps: p.video_kbps,
            audio_kbps: p.audio_kbps,
            expect_audio: true,
        };
        let _rt = ctx.rt.enter();
        match Feed::start(ctx.registry.clone(), cfg) {
            Ok(f) => {
                tracing::info!(stream = %p.stream, source = %p.source, "omt pull: publishing");
                PullStats::inc(&ctx.stats.publishes);
                self.feed = Some(f);
                self.map = TimeMap::new();
                self.busy_logged = false;
                true
            }
            Err(e) => {
                tracing::error!(stream = %p.stream, error = %e, "omt pull: cannot start the feed");
                ctx.stats.drop_frame(DropReason::Invalid);
                false
            }
        }
    }

    /// Counts a push result; a closed feed is dropped (restarted by the
    /// next frame).
    fn pushed(&mut self, r: Result<(), PushError>, ok: &AtomicU64) {
        let stats = &*self.ctx.stats;
        match r {
            Ok(()) => PullStats::inc(ok),
            Err(PushError::Full) => stats.drop_frame(DropReason::QueueFull),
            Err(PushError::Invalid) => stats.drop_frame(DropReason::Invalid),
            Err(PushError::Closed) => {
                tracing::warn!(stream = %self.ctx.pull.stream, "omt pull: feed closed; restarting it");
                self.feed = None;
            }
        }
    }

    fn on_video(&mut self, f: &OwnedFrame) {
        let stats = &*self.ctx.stats;
        if let Err(e) = self.decoder.decode_video(f, &mut self.video) {
            tracing::debug!(stream = %self.ctx.pull.stream, error = %e, "omt pull: video frame not decoded");
            stats.drop_frame(DropReason::Decode);
            return;
        }
        let v = &self.video;
        let ts = v.timestamp;
        let (Ok(width), Ok(height)) = (u32::try_from(v.width), u32::try_from(v.height)) else {
            stats.drop_frame(DropReason::Invalid);
            return;
        };
        let (fps_num, fps_den) = frame_rate(v.frame_rate_n, v.frame_rate_d);
        let format = VideoFormat { width, height, layout: PixelLayout::Uyvy, fps_num, fps_den };
        if PixelLayout::Uyvy.frame_len(width, height) != Some(v.data.len()) {
            stats.drop_frame(DropReason::Invalid);
            return;
        }
        if !self.ensure_feed() {
            return;
        }
        let frame_100ns = OMT_HZ * i64::from(fps_den) / i64::from(fps_num);
        let pts = self.map.video(ts, frame_100ns);
        // The decoder allocates a fresh buffer for the next picture.
        let data = Bytes::from(std::mem::take(&mut self.video.data));
        let r = self
            .feed
            .as_ref()
            .map_or(Err(PushError::Closed), |feed| feed.try_push_video(VideoFrame { format, pts, data }));
        self.pushed(r, &stats.video_pushed);
    }

    fn on_audio(&mut self, f: &OwnedFrame) {
        let stats = &*self.ctx.stats;
        if let Err(e) = media::decode_audio(f, &mut self.audio) {
            tracing::debug!(stream = %self.ctx.pull.stream, error = %e, "omt pull: audio frame not decoded");
            stats.drop_frame(DropReason::Decode);
            return;
        }
        let a = &self.audio;
        let (Ok(samples), Ok(rate)) = (u32::try_from(a.samples_per_channel), u32::try_from(a.sample_rate)) else {
            stats.drop_frame(DropReason::Invalid);
            return;
        };
        if samples == 0 || rate == 0 || a.channels == 0 {
            stats.drop_frame(DropReason::Invalid);
            return;
        }
        if !self.ensure_feed() {
            return;
        }
        let Some(pts) = self.map.audio(self.audio.timestamp, samples, rate) else {
            stats.drop_frame(DropReason::Timing);
            return;
        };
        let Some(chunk) = crate::audio::to_feed(&self.audio, pts) else {
            stats.drop_frame(DropReason::Invalid);
            return;
        };
        let r = self.feed.as_ref().map_or(Err(PushError::Closed), |feed| feed.try_push_audio(chunk));
        self.pushed(r, &stats.audio_pushed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(stream: &str) -> PullConfig {
        PullConfig {
            stream: stream.into(),
            source: "omt://127.0.0.1:6400".into(),
            quality: Quality::Default,
            video_kbps: 4000,
            audio_kbps: 128,
            ffmpeg: "ffmpeg".into(),
            directory: None,
        }
    }

    #[test]
    fn configs_compare_every_field() {
        assert_eq!(cfg("a"), cfg("a"));
        assert_ne!(cfg("a"), cfg("b"));
        assert_ne!(cfg("a"), PullConfig { video_kbps: 1, ..cfg("a") });
        assert_ne!(cfg("a"), PullConfig { quality: Quality::High, ..cfg("a") });
        let d = Arc::new(Directory::manual());
        let with = PullConfig { directory: Some(d.clone()), ..cfg("a") };
        assert_eq!(with, PullConfig { directory: Some(d), ..cfg("a") });
        assert_ne!(with, PullConfig { directory: Some(Arc::new(Directory::manual())), ..cfg("a") });
        assert_ne!(with, cfg("a"));
    }

    #[test]
    fn frame_rates_fall_back_when_invalid() {
        assert_eq!(frame_rate(60000, 1001), (60000, 1001));
        assert_eq!(frame_rate(0, 1), FALLBACK_FPS);
        assert_eq!(frame_rate(30, 0), FALLBACK_FPS);
        assert_eq!(frame_rate(-30, -1), FALLBACK_FPS);
        assert_eq!(frame_rate(1000, 1), FALLBACK_FPS);
    }

    #[test]
    fn tally_follows_the_stream() {
        let registry = Registry::new();
        assert_eq!(tally_for(&registry, "cam"), Tally::default());
        let publisher = registry.publish("cam", BufferConfig::default()).unwrap();
        assert_eq!(tally_for(&registry, "cam"), Tally { preview: true, program: false });
        let viewer = registry.subscribe("cam", caudal_core::StartAt::LiveEdge).unwrap();
        assert_eq!(tally_for(&registry, "cam"), Tally { preview: true, program: true });
        drop(viewer);
        publisher.stream().set_output_viewers("hls", 2);
        assert_eq!(tally_for(&registry, "cam"), Tally { preview: true, program: true });
        drop(publisher);
        assert_eq!(tally_for(&registry, "cam"), Tally::default());
    }

    #[test]
    fn tally_bits_round_trip() {
        let s = PullStats::default();
        for t in [
            Tally::default(),
            Tally { preview: true, program: false },
            Tally { preview: false, program: true },
            Tally { preview: true, program: true },
        ] {
            s.tally.store(tally_bits(t), Ordering::Relaxed);
            assert_eq!(s.tally(), t);
        }
    }

    #[test]
    fn drop_reasons_index_their_counters() {
        let s = PullStats::default();
        for (i, r) in DropReason::ALL.iter().enumerate() {
            assert_eq!(r.index(), i);
            s.drop_frame(*r);
        }
        s.drop_frame(DropReason::Busy);
        assert_eq!(s.dropped(DropReason::Busy), 2);
        assert_eq!(s.snapshot().dropped, [1, 1, 1, 1, 1, 2]);
    }
}
