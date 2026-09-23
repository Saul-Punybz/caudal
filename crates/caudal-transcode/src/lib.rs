//! Transcoding: each matching source stream `<name>` gets renditions
//! published as streams `<name>+<label>` (e.g. `main+480p`), which the
//! LL-HLS output groups into one multivariant playlist. Entry point fixed by
//! the orchestrator.
//!
//! Engines: `Ffmpeg` runs ffmpeg as an external process (never linked, so no
//! GPL in the binary), fed MPEG-TS on stdin and read as MPEG-TS on stdout via
//! `caudal-ts`; `RustyH264` encodes in-process with the pure-Rust
//! `rusty_h264` (video only; audio still needs ffmpeg or passthrough).
//!
//! How it runs (details in `NOTES.md`):
//!
//! - One task per source stream. Streams whose name contains `+` are never
//!   sources, so renditions are never transcoded again.
//! - `Ffmpeg`: ONE ffmpeg per source (one decode, `split` + `scale`, one
//!   libx264 + one AAC encode per rendition), writing every rendition into a
//!   single MPEG-TS on stdout, one PID per elementary stream; the reader
//!   splits packets by PID into one `caudal-ts` demuxer per rendition. The
//!   process lives in its own process group, killed (whole group) when the
//!   source ends or the task is dropped, and restarted with backoff if it
//!   dies while the source is live.
//! - Timestamps: frames go in shifted onto a private clock (`H` seconds of
//!   headroom after the source's first frame), ffmpeg runs with `-copyts`
//!   and `-mpegts_copyts 1`, and the output is shifted back, so renditions
//!   carry the source's own clock (same frame, same time) and players can
//!   switch between them cleanly.

mod ffmpeg;
mod mkv;
mod out;
pub mod pipe;
mod rusty;
mod scale;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use caudal_core::{Codec, Event, Registry, StartAt, Stream, Subscriber, TrackInfo, TrackKind};
use tokio::sync::broadcast::error::RecvError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendition {
    /// Suffix after `+`, e.g. `480p`. Must be a valid stream-name fragment.
    pub label: String,
    pub height: u32,
    pub video_kbps: u32,
    pub audio_kbps: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ladder {
    /// Source stream names or `prefix*` patterns.
    pub streams: Vec<String>,
    pub renditions: Vec<Rendition>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Ffmpeg,
    RustyH264,
}

#[derive(Debug, Clone)]
pub struct TranscodeConfig {
    pub ladders: Vec<Ladder>,
    pub engine: Engine,
    /// ffmpeg binary for the `Ffmpeg` engine (and audio with `RustyH264`).
    pub ffmpeg: PathBuf,
    pub buffer: caudal_core::BufferConfig,
}

fn validate(cfg: &TranscodeConfig) -> std::io::Result<()> {
    for ladder in &cfg.ladders {
        for r in &ladder.renditions {
            if r.label.is_empty() || !caudal_core::media::valid_stream_name(&r.label) || r.height < 2 {
                return Err(std::io::Error::other(format!("transcode: bad rendition `{}`", r.label)));
            }
        }
    }
    Ok(())
}

/// A handle to the running transcode subsystem. Cheap to clone.
///
/// [`TranscodeHandle::reload`] swaps the ladder config that new publishes
/// consult; a transcode already running for a source keeps the snapshot it
/// started with (its own `Arc<TranscodeConfig>`, loaded once in
/// [`consider`]), so an in-flight rendition is never interrupted by a
/// reload — only publishes from here on see the new ladders.
#[derive(Clone)]
pub struct TranscodeHandle {
    cfg: Arc<ArcSwap<TranscodeConfig>>,
}

impl TranscodeHandle {
    /// Validates `cfg` the same way [`start`] does, then swaps it in.
    /// Rejected (and the old config kept) if any rendition label is bad.
    pub fn reload(&self, cfg: TranscodeConfig) -> std::io::Result<()> {
        validate(&cfg)?;
        self.cfg.store(Arc::new(cfg));
        Ok(())
    }
}

/// Starts transcoding matching streams (current and future). Must be called
/// inside a tokio runtime.
pub fn start(registry: Arc<Registry>, cfg: TranscodeConfig) -> std::io::Result<TranscodeHandle> {
    tokio::runtime::Handle::try_current().map_err(|_| std::io::Error::other("transcode: no tokio runtime"))?;
    validate(&cfg)?;
    let cfg = Arc::new(ArcSwap::from_pointee(cfg));
    let active: Arc<Mutex<HashSet<usize>>> = Arc::default();
    // Subscribe before listing, so a stream published in between is seen
    // (twice at worst; `active` dedupes).
    let mut publishes = registry.subscribe_publishes();
    for s in registry.list() {
        consider(&registry, &cfg.load_full(), &active, s);
    }
    let handle = TranscodeHandle { cfg: cfg.clone() };
    tokio::spawn(async move {
        loop {
            match publishes.recv().await {
                Ok(s) => consider(&registry, &cfg.load_full(), &active, s),
                Err(RecvError::Lagged(_)) => {
                    for s in registry.list() {
                        consider(&registry, &cfg.load_full(), &active, s);
                    }
                }
                Err(RecvError::Closed) => break,
            }
        }
    });
    Ok(handle)
}

/// Does `pattern` (exact, `prefix*` or `*`) select stream `name`? Names
/// with `+` are renditions and never match.
pub(crate) fn matches(pattern: &str, name: &str) -> bool {
    if name.contains('+') {
        return false;
    }
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pattern == name,
    }
}

fn consider(registry: &Arc<Registry>, cfg: &Arc<TranscodeConfig>, active: &Arc<Mutex<HashSet<usize>>>, s: Arc<Stream>) {
    let name = s.name().to_owned();
    let Some(ladder) = cfg.ladders.iter().find(|l| l.streams.iter().any(|p| matches(p, &name))) else { return };
    if ladder.renditions.is_empty() || s.is_ended() {
        return;
    }
    let key = Arc::as_ptr(&s) as usize;
    if !active.lock().expect("poisoned").insert(key) {
        return;
    }
    let (registry, cfg, active, ladder) = (registry.clone(), cfg.clone(), active.clone(), ladder.clone());
    tokio::spawn(async move {
        run_source(registry, s, ladder, cfg).await;
        active.lock().expect("poisoned").remove(&key);
    });
}

/// What the engines need to know about a source.
pub(crate) struct Source {
    pub name: String,
    pub video: TrackInfo,
    /// The audio track the renditions carry (as AAC), if any.
    pub audio: Option<TrackInfo>,
    pub renditions: Vec<Rendition>,
}

impl Source {
    /// Source track by id, if it is one we feed.
    pub fn track(&self, id: caudal_core::TrackId) -> Option<&TrackInfo> {
        if self.video.id == id {
            return Some(&self.video);
        }
        self.audio.as_ref().filter(|a| a.id == id)
    }
}

/// Waits for the source's track list to carry a video track. Returns `None`
/// if the source ends first.
pub(crate) async fn wait_tracks(sub: &mut Subscriber) -> Option<Vec<TrackInfo>> {
    loop {
        let tracks = sub.tracks();
        if tracks.iter().any(|t| matches!(t.codec, Codec::H264 | Codec::H265)) {
            return Some(tracks);
        }
        match sub.recv().await {
            Event::End => return None,
            _ => continue,
        }
    }
}

async fn run_source(registry: Arc<Registry>, stream: Arc<Stream>, ladder: Ladder, cfg: Arc<TranscodeConfig>) {
    let name = stream.name().to_owned();
    let mut sub = stream.subscribe_internal(StartAt::LiveEdge);
    let Some(tracks) = wait_tracks(&mut sub).await else { return };
    let Some(src) = source_from_tracks(&name, &tracks, &ladder) else { return };
    tracing::info!(
        stream = %name,
        engine = ?cfg.engine,
        renditions = ?src.renditions.iter().map(|r| r.label.as_str()).collect::<Vec<_>>(),
        "transcode started"
    );
    match cfg.engine {
        Engine::Ffmpeg => ffmpeg::run(&registry, sub, src, &cfg).await,
        Engine::RustyH264 => rusty::run(&registry, sub, src, &cfg).await,
    }
    tracing::info!(stream = %name, "transcode ended");
}

/// Picks the video/audio tracks and the renditions worth producing (those
/// strictly smaller than the source; the others are logged once and skipped).
pub(crate) fn source_from_tracks(name: &str, tracks: &[TrackInfo], ladder: &Ladder) -> Option<Source> {
    let video = tracks.iter().find(|t| matches!(t.codec, Codec::H264 | Codec::H265))?.clone();
    let audio = tracks.iter().find(|t| t.kind() == TrackKind::Audio).cloned();
    let src_h = video.video.map_or(0, |v| v.height);
    let mut renditions = Vec::new();
    for r in &ladder.renditions {
        if src_h > 0 && r.height >= src_h {
            tracing::info!(stream = %name, rendition = %r.label, height = r.height, source_height = src_h,
                "rendition skipped: not smaller than the source");
        } else {
            renditions.push(r.clone());
        }
    }
    if renditions.is_empty() {
        tracing::info!(stream = %name, "no rendition to produce");
        return None;
    }
    Some(Source { name: name.to_owned(), video, audio, renditions })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns() {
        assert!(matches("main", "main"));
        assert!(!matches("main", "main2"));
        assert!(matches("cam*", "cam_1"));
        assert!(matches("*", "anything"));
        assert!(!matches("*", "main+480p"));
        assert!(!matches("main*", "main+480p"));
        assert!(!matches("main+480p", "main+480p"));
    }
}

#[cfg(test)]
mod reload_tests {
    use super::*;

    fn one_ladder(label: &str, height: u32) -> TranscodeConfig {
        TranscodeConfig {
            ladders: vec![Ladder {
                streams: vec!["src".into()],
                renditions: vec![Rendition { label: label.into(), height, video_kbps: 500, audio_kbps: 128 }],
            }],
            engine: Engine::Ffmpeg,
            ffmpeg: "ffmpeg".into(),
            buffer: caudal_core::BufferConfig::default(),
        }
    }

    #[tokio::test]
    async fn reload_swaps_the_snapshot_new_publishes_will_load() {
        let registry = Registry::new();
        let handle = start(registry, one_ladder("240p", 240)).unwrap();
        let before = handle.cfg.load_full();
        handle.reload(one_ladder("480p", 480)).unwrap();
        let after = handle.cfg.load_full();
        assert!(!Arc::ptr_eq(&before, &after), "reload must publish a new snapshot");
        assert_eq!(after.ladders[0].renditions[0].label, "480p");
    }

    #[tokio::test]
    async fn reload_rejects_a_bad_rendition_and_keeps_the_running_config() {
        let registry = Registry::new();
        let handle = start(registry, one_ladder("240p", 240)).unwrap();
        let before = handle.cfg.load_full();
        let err = handle.reload(one_ladder("", 240)).unwrap_err();
        assert!(err.to_string().contains("bad rendition"), "{err}");
        let after = handle.cfg.load_full();
        assert!(Arc::ptr_eq(&before, &after), "an invalid reload must not touch the running config");
    }
}
