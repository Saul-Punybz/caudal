//! Automatic live captions: each matching stream's audio is decoded to
//! 16 kHz mono, cut into chunks at pauses, transcribed locally by Whisper
//! (on `candle`, no cloud service), and turned into caption cues that the
//! outputs read through [`caudal_core::captions::CaptionSource`] (LL-HLS
//! serves them as a WebVTT rendition).
//!
//! How it runs (design note: `docs/research/CAPTIONS.md`):
//!
//! - Per captioned stream: one tokio task reads frames and hands the audio
//!   track's access units to a bounded queue; one OS thread decodes them
//!   (`audio`), cuts chunks (`chunk`) and offers each to the engine.
//! - One engine for the whole server: a single worker thread running
//!   inference on a rayon pool of exactly `threads` threads (the CPU
//!   budget). While it is busy, a stream's new chunks join that stream's
//!   waiting job (up to Whisper's 30 s window), so a slow machine runs
//!   fewer, longer inferences instead of falling behind. Its queue is
//!   bounded; a chunk that does not fit, or a job that waited too long, is
//!   dropped and counted. Ingest never waits on it.
//! - A panic inside inference is caught; the worker reloads its model
//!   state from the pristine copy and carries on. The model loads in the
//!   background; a missing model is logged with the command that fetches
//!   it, and the server runs on without captions.

pub mod audio;
pub mod chunk;
pub mod cues;
pub mod eval;
pub mod mel;
pub mod models;
pub mod tokenizer;
pub mod whisper;

use std::collections::{HashMap, VecDeque};
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, TrySendError};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use caudal_core::captions::{CaptionSource, TextCue, TextTrack};
use caudal_core::{Codec, Event, Registry, StartAt, Stream, TrackKind};
use parking_lot::{Condvar, Mutex};
use tokio::sync::broadcast::error::RecvError;

use crate::chunk::{Chunk, ChunkConfig, Chunker};
use crate::cues::CueStore;
use crate::mel::{N_SAMPLES, SAMPLE_RATE};
pub use crate::whisper::{DeviceChoice, Language};
use crate::whisper::{EngineError, Model, Transcript};

/// Access units waiting for the decoder thread of one stream (~44 s of
/// AAC at 48 kHz). When full, audio is dropped and counted. At 512 the
/// decoder thread, starved of CPU by a busy inference pool, lost 6.5 s of
/// a 4x-real-time test stream, and every hole cut a short chunk.
const AUDIO_QUEUE: usize = 2048;
/// Jobs waiting for the engine, across all streams. A stream's chunks join
/// its waiting job while that has room, so one stream rarely fills it.
const JOB_QUEUE: usize = 4;
/// A job holds at most one Whisper window (30 s) of audio. Whisper encodes
/// the full window whatever the length, so a fuller job costs little more
/// than a short chunk.
const JOB_SAMPLES: usize = N_SAMPLES;
/// A job that waited this long is no longer worth captioning.
const STALE: Duration = Duration::from_secs(10);
/// A chunk shorter than this (a gap in the input closed it early) is not
/// worth an inference on its own: it waits for the audio after it...
const MIN_ALONE_US: i64 = 1_000_000;
/// ...but no longer than this.
const HOLD: Duration = Duration::from_secs(1);
/// Consecutive audio timestamps further apart than this (or going
/// backwards) are a gap: the chunk so far is closed and the clock re-anchored.
const GAP_US: i64 = 150_000;

#[derive(Debug, Clone, PartialEq)]
pub struct StreamRule {
    /// Stream names or `prefix*` patterns (`*` for all). Renditions
    /// (`name+label`) never match; they share their source's captions.
    pub streams: Vec<String>,
    pub language: Language,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CaptionsConfig {
    pub model_dir: PathBuf,
    /// `tiny`, `base`, `small` (fetchable) or the name of any
    /// `whisper-<name>` directory under `model_dir`.
    pub model: String,
    /// Inference threads: the hard cap on the cores captioning can use.
    pub threads: usize,
    pub device: DeviceChoice,
    /// Streams captioned at once; later ones get none (logged).
    pub max_streams: usize,
    /// The last cue of a chunk stays up at least this long.
    pub min_display_ms: u32,
    pub rules: Vec<StreamRule>,
}

/// Does `pattern` (exact, `prefix*` or `*`) select stream `name`?
pub fn matches(pattern: &str, name: &str) -> bool {
    if name.contains('+') {
        return false;
    }
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pattern == name,
    }
}

/// Counters for one captioned stream (for `/metrics`).
#[derive(Default)]
struct Counters {
    chunks: AtomicU64,
    inferences: AtomicU64,
    cues: AtomicU64,
    dropped_audio_ms: AtomicU64,
    dropped_chunks: AtomicU64,
    /// Inference time and audio transcribed, all jobs so far, in µs.
    infer_us: AtomicU64,
    audio_us: AtomicU64,
    /// Last job's inference time / its duration, x1000.
    last_rtf_milli: AtomicU64,
    /// Media time from the end of the last chunk's speech to its text
    /// being placed, in ms.
    latency_ms: AtomicU64,
}

struct Captioned {
    store: Mutex<CueStore>,
    counters: Counters,
    /// The publish this state belongs to (a republish replaces it).
    live: Weak<Stream>,
    detected: Mutex<Option<String>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StreamMetrics {
    pub stream: String,
    /// Chunks transcribed (several can share one inference run).
    pub chunks: u64,
    /// Inference runs.
    pub inferences: u64,
    pub cues: u64,
    pub dropped_audio_seconds: f64,
    pub dropped_chunks: u64,
    /// Inference time / audio time over everything transcribed so far.
    pub real_time_factor: f64,
    /// The same for the last inference run alone.
    pub last_real_time_factor: f64,
    pub latency_seconds: f64,
}

/// Audio on its way to the engine: one chunk, or consecutive chunks of one
/// stream joined together.
struct Piece {
    /// Media time the last chunk ends.
    end_us: i64,
    pcm: Vec<f32>,
    chunks: u64,
}

impl Piece {
    fn audio_us(&self) -> i64 {
        self.pcm.len() as i64 * 1_000_000 / SAMPLE_RATE as i64
    }

    fn join(&mut self, next: Piece) {
        self.pcm.extend_from_slice(&next.pcm);
        self.end_us = next.end_us;
        self.chunks += next.chunks;
    }
}

struct Job {
    state: Arc<Captioned>,
    live: Arc<Stream>,
    language: Language,
    piece: Piece,
    queued: Instant,
}

/// The engine's job queue: bounded, and a stream's new audio joins its
/// newest waiting job while that has room.
#[derive(Default)]
struct JobQueue {
    jobs: Mutex<VecDeque<Job>>,
    ready: Condvar,
    closed: AtomicBool,
}

impl JobQueue {
    /// Queues `piece`; gives it back when there is no room.
    fn offer(
        &self,
        state: &Arc<Captioned>,
        live: &Arc<Stream>,
        language: &Language,
        piece: Piece,
    ) -> Result<(), Piece> {
        let mut jobs = self.jobs.lock();
        if let Some(job) = jobs.iter_mut().rev().find(|j| Arc::ptr_eq(&j.state, state))
            && job.piece.pcm.len() + piece.pcm.len() <= JOB_SAMPLES
        {
            job.piece.join(piece);
            return Ok(());
        }
        if jobs.len() >= JOB_QUEUE {
            return Err(piece);
        }
        jobs.push_back(Job {
            state: state.clone(),
            live: live.clone(),
            language: language.clone(),
            piece,
            queued: Instant::now(),
        });
        self.ready.notify_one();
        Ok(())
    }

    /// The oldest job; `None` once the queue is closed.
    fn next(&self) -> Option<Job> {
        let mut jobs = self.jobs.lock();
        loop {
            if self.closed.load(Ordering::Relaxed) {
                return None;
            }
            if let Some(job) = jobs.pop_front() {
                return Some(job);
            }
            self.ready.wait(&mut jobs);
        }
    }

    fn close(&self) {
        let _jobs = self.jobs.lock();
        self.closed.store(true, Ordering::Relaxed);
        self.ready.notify_all();
    }
}

struct Inner {
    cfg: CaptionsConfig,
    streams: Mutex<HashMap<String, Arc<Captioned>>>,
    queue: Arc<JobQueue>,
    model_ready: AtomicBool,
    model_failed: AtomicBool,
    panics: AtomicU64,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.queue.close();
    }
}

/// Speech-to-text as the engine thread sees it: Whisper in production, a
/// stand-in with a known cost in the pipeline tests.
pub trait Recognizer: Send {
    /// Transcribes one span of 16 kHz mono audio (at most 30 s).
    fn transcribe(&mut self, pcm: &[f32], language: &Language) -> Result<Transcript, EngineError>;
    /// Back to the freshly loaded state (after a panic mid-inference).
    fn reset(&mut self);
}

/// Builds the recogniser on the engine thread, inside the inference pool.
/// An error is logged and leaves the server running without captions.
pub type Loader = Box<dyn FnOnce(&CaptionsConfig) -> Result<Box<dyn Recognizer>, String> + Send>;

/// Whisper plus the pristine copy a panic restores it from.
struct Whisper {
    pristine: Model,
    model: Model,
}

impl Recognizer for Whisper {
    fn transcribe(&mut self, pcm: &[f32], language: &Language) -> Result<Transcript, EngineError> {
        self.model.transcribe(pcm, language)
    }

    fn reset(&mut self) {
        self.model = self.pristine.clone();
    }
}

fn load_whisper(cfg: &CaptionsConfig) -> Result<Box<dyn Recognizer>, String> {
    let dir = models::check(&cfg.model_dir, &cfg.model)?;
    let t0 = Instant::now();
    let pristine = Model::load(&dir, cfg.device).map_err(|e| format!("cannot load model {}: {e}", dir.display()))?;
    for r in &cfg.rules {
        if let Language::Fixed(code) = &r.language
            && !pristine.knows_language(code)
        {
            return Err(format!("model `{}` does not know language `{code}`", cfg.model));
        }
    }
    tracing::info!(model = %cfg.model, device = pristine.device_name(), threads = cfg.threads, secs = t0.elapsed().as_secs_f32(), "captions: model loaded");
    Ok(Box::new(Whisper { model: pristine.clone(), pristine }))
}

/// The running captions subsystem. Cheap to clone.
#[derive(Clone)]
pub struct Captions {
    inner: Arc<Inner>,
}

impl Captions {
    /// Starts captioning matching streams (current and future) and loads
    /// the model in the background. Must be called inside a tokio runtime.
    pub fn start(registry: Arc<Registry>, cfg: CaptionsConfig) -> Self {
        Self::start_with(registry, cfg, Box::new(load_whisper))
    }

    /// [`Captions::start`] with another recogniser (pipeline tests).
    pub fn start_with(registry: Arc<Registry>, cfg: CaptionsConfig, load: Loader) -> Self {
        let queue = Arc::new(JobQueue::default());
        let inner = Arc::new(Inner {
            cfg,
            streams: Mutex::default(),
            queue: queue.clone(),
            model_ready: AtomicBool::new(false),
            model_failed: AtomicBool::new(false),
            panics: AtomicU64::new(0),
        });
        let weak = Arc::downgrade(&inner);
        let spawned =
            std::thread::Builder::new().name("captions-engine".into()).spawn(move || engine(weak, &queue, load));
        if let Err(e) = spawned {
            tracing::error!(error = %e, "captions: cannot start the engine thread");
        }
        let me = Self { inner };
        let mut publishes = registry.subscribe_publishes();
        for s in registry.list() {
            me.consider(s);
        }
        let this = me.clone();
        tokio::spawn(async move {
            loop {
                match publishes.recv().await {
                    Ok(s) => this.consider(s),
                    Err(RecvError::Lagged(_)) => {
                        for s in registry.list() {
                            this.consider(s);
                        }
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        });
        me
    }

    fn rule(&self, name: &str) -> Option<&StreamRule> {
        self.inner.cfg.rules.iter().find(|r| r.streams.iter().any(|p| matches(p, name)))
    }

    fn consider(&self, live: Arc<Stream>) {
        let name = live.name().to_owned();
        let Some(rule) = self.rule(&name) else { return };
        let language = rule.language.clone();
        let state = {
            let mut map = self.inner.streams.lock();
            map.retain(|_, s| s.live.upgrade().is_some_and(|l| !l.is_ended()));
            if let Some(s) = map.get(&name)
                && s.live.upgrade().is_some_and(|l| Arc::ptr_eq(&l, &live))
            {
                return;
            }
            if !map.contains_key(&name) && map.len() >= self.inner.cfg.max_streams {
                tracing::warn!(stream = %name, max = self.inner.cfg.max_streams, "captions: max_streams reached; this stream gets no captions");
                return;
            }
            let state = Arc::new(Captioned {
                store: Mutex::default(),
                counters: Counters::default(),
                live: Arc::downgrade(&live),
                detected: Mutex::new(None),
            });
            map.insert(name.clone(), state.clone());
            state
        };
        tracing::info!(stream = %name, ?language, "captions: started");
        // Subscribe now, not when the task first runs, so no audio pushed
        // in between is missed.
        let sub = live.subscribe_internal(StartAt::LiveEdge);
        tokio::spawn(feed(self.inner.clone(), state, sub, language));
    }

    /// Per-stream counters for `/metrics`.
    pub fn metrics(&self) -> Vec<StreamMetrics> {
        let map = self.inner.streams.lock();
        let mut out: Vec<StreamMetrics> = map
            .iter()
            .map(|(name, s)| {
                let c = &s.counters;
                let audio_us = c.audio_us.load(Ordering::Relaxed);
                StreamMetrics {
                    stream: name.clone(),
                    chunks: c.chunks.load(Ordering::Relaxed),
                    inferences: c.inferences.load(Ordering::Relaxed),
                    cues: c.cues.load(Ordering::Relaxed),
                    dropped_audio_seconds: c.dropped_audio_ms.load(Ordering::Relaxed) as f64 / 1000.0,
                    dropped_chunks: c.dropped_chunks.load(Ordering::Relaxed),
                    real_time_factor: if audio_us == 0 {
                        0.0
                    } else {
                        c.infer_us.load(Ordering::Relaxed) as f64 / audio_us as f64
                    },
                    last_real_time_factor: c.last_rtf_milli.load(Ordering::Relaxed) as f64 / 1000.0,
                    latency_seconds: c.latency_ms.load(Ordering::Relaxed) as f64 / 1000.0,
                }
            })
            .collect();
        out.sort_by(|a, b| a.stream.cmp(&b.stream));
        out
    }

    /// True once the model is loaded and transcribing.
    pub fn model_ready(&self) -> bool {
        self.inner.model_ready.load(Ordering::Relaxed)
    }

    /// Inference panics caught (and recovered from) since start.
    pub fn engine_panics(&self) -> u64 {
        self.inner.panics.load(Ordering::Relaxed)
    }
}

impl CaptionSource for Captions {
    fn track(&self, stream: &str) -> Option<TextTrack> {
        let rule = self.rule(stream)?;
        {
            let map = self.inner.streams.lock();
            if !map.contains_key(stream) && map.len() >= self.inner.cfg.max_streams {
                return None;
            }
        }
        let (language, name) = match &rule.language {
            Language::Fixed(code) => (Some(code.clone()), language_name(code)),
            Language::Auto => (None, "Auto".to_owned()),
        };
        Some(TextTrack { language, name: format!("{name} (auto)") })
    }

    fn cues(&self, stream: &str, from_us: i64, to_us: i64) -> Vec<TextCue> {
        let state = self.inner.streams.lock().get(stream).cloned();
        state.map(|s| s.store.lock().overlapping(from_us, to_us)).unwrap_or_default()
    }
}

fn language_name(code: &str) -> String {
    match code {
        "es" => "Español".into(),
        "en" => "English".into(),
        other => other.to_owned(),
    }
}

enum AudioMsg {
    Track(caudal_core::TrackInfo),
    Frame { pts_us: i64, data: bytes::Bytes },
    Gap,
}

/// Reads the stream and hands its audio to the decoder thread. Never
/// waits on it: a full queue drops the frame.
async fn feed(inner: Arc<Inner>, state: Arc<Captioned>, mut sub: caudal_core::Subscriber, language: Language) {
    let live = sub.stream().clone();
    let (tx, rx) = std::sync::mpsc::sync_channel::<AudioMsg>(AUDIO_QUEUE);
    let name = live.name().to_owned();
    {
        let (inner, state, live) = (inner.clone(), state.clone(), live.clone());
        let spawned = std::thread::Builder::new()
            .name(format!("captions-{name}"))
            .spawn(move || decode_loop(&inner, &state, &live, &language, &rx));
        if let Err(e) = spawned {
            tracing::error!(stream = %name, error = %e, "captions: cannot start the decoder thread");
            return;
        }
    }
    let mut audio: Option<caudal_core::TrackInfo> = None;
    let mut warned = false;
    loop {
        let msg = match sub.recv().await {
            Event::TracksChanged => {
                let tracks = sub.tracks();
                let pick = tracks
                    .iter()
                    .find(|t| t.kind() == TrackKind::Audio && matches!(t.codec, Codec::Aac | Codec::Opus))
                    .cloned();
                if pick.is_none() && tracks.iter().any(|t| t.kind() == TrackKind::Audio) {
                    tracing::warn!(stream = %name, "captions: audio codec not supported (AAC or Opus only)");
                }
                audio = pick.clone();
                match pick {
                    Some(t) => AudioMsg::Track(t),
                    None => continue,
                }
            }
            Event::Frame(f) => match &audio {
                Some(t) if t.id == f.track => AudioMsg::Frame { pts_us: t.to_micros(f.pts), data: f.data.clone() },
                _ => continue,
            },
            Event::Lagged { .. } => AudioMsg::Gap,
            Event::Cue(_) => continue,
            Event::End => break,
        };
        match tx.try_send(msg) {
            Ok(()) => {}
            Err(TrySendError::Full(AudioMsg::Frame { .. })) => {
                // One AAC frame at 48 kHz: ~21 ms.
                state.counters.dropped_audio_ms.fetch_add(21, Ordering::Relaxed);
                if !warned {
                    warned = true;
                    tracing::warn!(stream = %name, "captions: decoder behind; dropping audio");
                }
            }
            Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Disconnected(_)) => break,
        }
    }
    // Dropping `tx` ends the decoder thread once it drains.
    drop(tx);
}

/// A chunk too short to transcribe alone, waiting (at most [`HOLD`]) for
/// the audio after it.
#[derive(Default)]
struct Held(Option<(Piece, Instant)>);

impl Held {
    /// Adds `piece` to what is held; returns what is worth transcribing.
    fn add(&mut self, piece: Piece) -> Option<Piece> {
        let (piece, since) = match self.0.take() {
            Some((mut held, since)) => {
                held.join(piece);
                (held, since)
            }
            None => (piece, Instant::now()),
        };
        if piece.audio_us() < MIN_ALONE_US {
            self.0 = Some((piece, since));
            return None;
        }
        Some(piece)
    }

    /// How long what is held may still wait.
    fn wait(&self) -> Option<Duration> {
        self.0.as_ref().map(|(_, since)| HOLD.saturating_sub(since.elapsed()))
    }

    /// What is held, once it has waited long enough (or right away).
    fn release(&mut self, now: bool) -> Option<Piece> {
        if now || self.wait().is_some_and(|w| w.is_zero()) { self.0.take().map(|(p, _)| p) } else { None }
    }
}

/// The decoder thread of one stream: access units → PCM → chunks → jobs.
fn decode_loop(
    inner: &Inner,
    state: &Arc<Captioned>,
    live: &Arc<Stream>,
    language: &Language,
    rx: &Receiver<AudioMsg>,
) {
    let mut decoder: Option<audio::AudioDecoder> = None;
    let mut chunker = Chunker::new(ChunkConfig::default());
    // (sample index, media µs) the chunker's clock is anchored at.
    let mut anchor: Option<(u64, i64)> = None;
    let mut last_pts: Option<i64> = None;
    let mut held = Held::default();
    let submit = |held: &mut Held, chunk: Chunk, anchor: (u64, i64)| {
        let start_us = anchor.1 + ((chunk.start - anchor.0) as i64 * 1_000_000 / SAMPLE_RATE as i64);
        let end_us = start_us + chunk.duration_us();
        if let Some(p) = held.add(Piece { end_us, pcm: chunk.pcm, chunks: 1 }) {
            offer(inner, state, live, language, p);
        }
    };
    loop {
        let msg = match held.wait() {
            Some(wait) => match rx.recv_timeout(wait) {
                Ok(m) => Some(m),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            },
            None => match rx.recv() {
                Ok(m) => Some(m),
                Err(_) => break,
            },
        };
        if let Some(p) = held.release(false) {
            offer(inner, state, live, language, p);
        }
        let Some(msg) = msg else { continue };
        match msg {
            AudioMsg::Track(t) => {
                if let (Some(a), Some(c)) = (anchor, chunker.flush()) {
                    submit(&mut held, c, a);
                }
                decoder = audio::AudioDecoder::new(&t);
                if decoder.is_none() {
                    tracing::warn!(stream = %live.name(), codec = ?t.codec, "captions: cannot configure the audio decoder");
                }
                anchor = None;
                last_pts = None;
            }
            AudioMsg::Gap => last_pts = None,
            AudioMsg::Frame { pts_us, data } => {
                let Some(dec) = decoder.as_mut() else { continue };
                let gap = last_pts.is_none_or(|p| pts_us <= p || pts_us - p > GAP_US);
                last_pts = Some(pts_us);
                if gap {
                    if let (Some(a), Some(c)) = (anchor, chunker.flush()) {
                        submit(&mut held, c, a);
                    }
                    let pos = chunker.position();
                    chunker.reset(pos);
                    anchor = Some((pos, pts_us));
                }
                let pcm = match dec.decode(&data) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::debug!(stream = %live.name(), error = %e, "captions: undecodable audio frame");
                        continue;
                    }
                };
                let a = anchor.expect("anchored above");
                for c in chunker.push(&pcm) {
                    submit(&mut held, c, a);
                }
            }
        }
    }
    if let (Some(a), Some(c)) = (anchor, chunker.flush()) {
        submit(&mut held, c, a);
    }
    if let Some(p) = held.release(true) {
        offer(inner, state, live, language, p);
    }
}

fn offer(inner: &Inner, state: &Arc<Captioned>, live: &Arc<Stream>, language: &Language, piece: Piece) {
    if inner.model_failed.load(Ordering::Relaxed) {
        return;
    }
    if let Err(piece) = inner.queue.offer(state, live, language, piece) {
        let ms = piece.audio_us() as u64 / 1000;
        state.counters.dropped_chunks.fetch_add(piece.chunks, Ordering::Relaxed);
        state.counters.dropped_audio_ms.fetch_add(ms, Ordering::Relaxed);
        tracing::warn!(stream = %live.name(), ms, "captions: transcription behind; chunk dropped");
    }
}

/// The engine thread: loads the model, then runs jobs one at a time on a
/// pool of `threads` threads.
fn engine(inner: Weak<Inner>, queue: &JobQueue, load: Loader) {
    let cfg = match inner.upgrade() {
        Some(i) => i.cfg.clone(),
        None => return,
    };
    let fail = |msg: String| {
        tracing::error!("captions: {msg}; streams keep playing without captions");
        if let Some(i) = inner.upgrade() {
            i.model_failed.store(true, Ordering::Relaxed);
        }
    };
    let threads = cfg.threads.clamp(1, 64);
    let pool = match rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("captions-infer-{i}"))
        .build()
    {
        Ok(p) => p,
        Err(e) => return fail(format!("cannot build the inference pool: {e}")),
    };
    let mut model = match pool.install(|| load(&cfg)) {
        Ok(m) => m,
        Err(e) => return fail(e),
    };
    match inner.upgrade() {
        Some(i) => i.model_ready.store(true, Ordering::Relaxed),
        None => return,
    }
    while let Some(job) = queue.next() {
        let Some(inner) = inner.upgrade() else { return };
        run_job(&inner, &pool, model.as_mut(), job, cfg.min_display_ms);
    }
}

fn run_job(inner: &Inner, pool: &rayon::ThreadPool, model: &mut dyn Recognizer, job: Job, min_display_ms: u32) {
    let c = &job.state.counters;
    let piece = &job.piece;
    let dur_us = piece.audio_us();
    if job.queued.elapsed() > STALE || job.live.is_ended() {
        c.dropped_chunks.fetch_add(piece.chunks, Ordering::Relaxed);
        c.dropped_audio_ms.fetch_add(dur_us as u64 / 1000, Ordering::Relaxed);
        return;
    }
    let t = Instant::now();
    let result =
        std::panic::catch_unwind(AssertUnwindSafe(|| pool.install(|| model.transcribe(&piece.pcm, &job.language))));
    let spent = t.elapsed();
    c.chunks.fetch_add(piece.chunks, Ordering::Relaxed);
    c.inferences.fetch_add(1, Ordering::Relaxed);
    c.infer_us.fetch_add(spent.as_micros() as u64, Ordering::Relaxed);
    c.audio_us.fetch_add(dur_us.max(0) as u64, Ordering::Relaxed);
    c.last_rtf_milli.store((spent.as_secs_f64() * 1e9 / dur_us.max(1) as f64) as u64, Ordering::Relaxed);
    let text: Transcript = match result {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => {
            tracing::warn!(stream = %job.live.name(), error = %e, "captions: transcription failed");
            return;
        }
        Err(_) => {
            inner.panics.fetch_add(1, Ordering::Relaxed);
            tracing::error!(stream = %job.live.name(), "captions: inference panicked; model state reset");
            model.reset();
            return;
        }
    };
    if text.is_silence() {
        return;
    }
    if job.language == Language::Auto {
        *job.state.detected.lock() = Some(text.language.clone());
    }
    let speech_end = piece.end_us;
    // The live edge now: the earliest time a player can still be shown.
    let edge = job.live.newest_micros().unwrap_or(speech_end).max(speech_end);
    c.latency_ms.store(((edge - speech_end).max(0) / 1000) as u64, Ordering::Relaxed);
    let mut store = job.state.store.lock();
    // After the previous chunk's reading time, so cues follow each other
    // like the speech did.
    let start = store.next_free().map_or(edge, |f| f.max(edge));
    let laid = cues::layout(&text.text, start, dur_us, i64::from(min_display_ms) * 1000);
    c.cues.fetch_add(laid.cues.len() as u64, Ordering::Relaxed);
    tracing::debug!(stream = %job.live.name(), text = %text.text, "captions: cue");
    store.add(laid);
}

/// Reads a 16-bit PCM WAV file as mono f32 samples plus its sample rate.
/// For tests and the `transcribe` example; live audio never goes through
/// a file.
pub fn read_wav(bytes: &[u8]) -> Option<(Vec<f32>, u32)> {
    if bytes.get(0..4)? != b"RIFF" || bytes.get(8..12)? != b"WAVE" {
        return None;
    }
    let (mut pos, mut fmt) = (12, None);
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().ok()?) as usize;
        let body = bytes.get(pos + 8..(pos + 8 + len).min(bytes.len()))?;
        match id {
            b"fmt " => {
                let format = u16::from_le_bytes(body.get(0..2)?.try_into().ok()?);
                let channels = u16::from_le_bytes(body.get(2..4)?.try_into().ok()?);
                let rate = u32::from_le_bytes(body.get(4..8)?.try_into().ok()?);
                let bits = u16::from_le_bytes(body.get(14..16)?.try_into().ok()?);
                if format != 1 || bits != 16 || channels == 0 {
                    return None;
                }
                fmt = Some((channels as usize, rate));
            }
            b"data" => {
                let (ch, rate) = fmt?;
                let mono = body
                    .chunks_exact(2 * ch)
                    .map(|f| {
                        f.as_chunks::<2>().0.iter().map(|s| f32::from(i16::from_le_bytes(*s)) / 32768.0).sum::<f32>()
                            / ch as f32
                    })
                    .collect();
                return Some((mono, rate));
            }
            _ => {}
        }
        pos += 8 + len + (len & 1);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_select_sources_not_renditions() {
        assert!(matches("*", "main"));
        assert!(matches("news*", "news-es"));
        assert!(matches("main", "main"));
        assert!(!matches("main", "main2"));
        assert!(!matches("*", "main+480p"));
    }

    #[test]
    fn wav_reader_reads_pcm16() {
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36u32 + 8).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&2u16.to_le_bytes()); // stereo
        wav.extend_from_slice(&16_000u32.to_le_bytes());
        wav.extend_from_slice(&64_000u32.to_le_bytes());
        wav.extend_from_slice(&4u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&8u32.to_le_bytes());
        for s in [16384i16, 0, -16384, -16384] {
            wav.extend_from_slice(&s.to_le_bytes());
        }
        let (pcm, rate) = read_wav(&wav).unwrap();
        assert_eq!(rate, 16_000);
        assert_eq!(pcm, vec![0.25, -0.5]);
    }
}
