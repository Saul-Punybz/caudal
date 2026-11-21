//! `[[rtmp.pull]]`: Caudal connects to a remote RTMP/RTMPS server as a
//! player (`connect`, `createStream`, `play` the URL's last path segment)
//! and publishes the FLV audio/video it receives as `stream`, through the
//! same [`FlvIngest`] an RTMP publisher connecting in goes through (so
//! H.264, AAC and Enhanced RTMP HEVC/AV1 work alike).
//!
//! - The stream is published when the first media message arrives, so a
//!   server without the stream leaves no empty stream behind.
//! - [`IDLE`] without media, `NetStream.Play.Stop`/`.Complete`/
//!   `.UnpublishNotify`, or the connection closing ends the stream; the
//!   pull then reconnects with backoff ([`BACKOFF_MIN`]..[`BACKOFF_MAX`],
//!   back to the minimum once media flowed).
//! - The stream key never reaches a log line: logs print
//!   [`redact_url`]'s output.

use std::sync::Arc;
use std::time::Duration;

use caudal_core::{BufferConfig, PublishError, Registry};
use parking_lot::Mutex;

use crate::client::{PlayEvent, RtmpClient};
use crate::ingest::FlvIngest;
use crate::url::{ParsedUrl, parse_target_url, redact_url};

/// A live stream that sends no media for this long has stopped.
pub const IDLE: Duration = Duration::from_secs(5);
pub const BACKOFF_MIN: Duration = Duration::from_secs(1);
pub const BACKOFF_MAX: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// One `[[rtmp.pull]]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtmpPull {
    pub stream: String,
    pub url: String,
}

/// Checks a pull URL: `rtmp://` or `rtmps://host[:port]/app/key`.
pub fn parse_url(url: &str) -> Result<ParsedUrl, String> {
    parse_target_url(url).map_err(|e| format!("{:?}: {e}", redact_url(url)))
}

enum Outcome {
    /// Media flowed, then stopped.
    Flowed,
    /// No media: the connect, the handshake or the `play` failed.
    Failed,
    /// Nothing will ever work (bad name): stop the pull.
    Fatal,
}

/// What to do with one event; `Some` ends the session.
fn handle(
    ev: PlayEvent,
    pull: &RtmpPull,
    registry: &Arc<Registry>,
    buffer: BufferConfig,
    ingest: &mut Option<FlvIngest>,
    fps: &mut Option<f64>,
) -> Option<Outcome> {
    let media = matches!(ev, PlayEvent::Video { .. } | PlayEvent::Audio { .. });
    if media && ingest.is_none() {
        match registry.publish(&pull.stream, buffer) {
            Ok(p) => {
                tracing::info!(stream = %pull.stream, url = %redact_url(&pull.url), "rtmp pull: media arriving; published");
                let i = FlvIngest::new(p);
                if let Some(f) = *fps {
                    i.set_fps(f);
                }
                *ingest = Some(i);
            }
            Err(PublishError::Busy(_)) => {
                tracing::warn!(stream = %pull.stream, "rtmp pull: stream already has a publisher; retrying");
                return Some(Outcome::Failed);
            }
            Err(PublishError::InvalidName) => {
                tracing::error!(stream = %pull.stream, "rtmp pull: invalid stream name; stopped");
                return Some(Outcome::Fatal);
            }
        }
    }
    match ev {
        PlayEvent::Started => {
            tracing::debug!(stream = %pull.stream, "rtmp pull: NetStream.Play.Start");
        }
        PlayEvent::Video { timestamp, data } => ingest.as_ref()?.video(timestamp, data),
        PlayEvent::Audio { timestamp, data } => ingest.as_ref()?.audio(timestamp, data),
        PlayEvent::Metadata(m) => {
            if let Some(f) = m.video_frame_rate.filter(|f| *f > 0.0) {
                *fps = Some(f64::from(f));
                if let Some(i) = ingest.as_ref() {
                    i.set_fps(f64::from(f));
                }
            }
        }
        PlayEvent::Ended(code) => {
            tracing::info!(stream = %pull.stream, %code, "rtmp pull: the server ended the stream");
            return Some(if ingest.is_some() { Outcome::Flowed } else { Outcome::Failed });
        }
        PlayEvent::Refused(reason) => {
            tracing::warn!(stream = %pull.stream, url = %redact_url(&pull.url), %reason, "rtmp pull: play refused");
            return Some(if ingest.is_some() { Outcome::Flowed } else { Outcome::Failed });
        }
    }
    None
}

/// One connection: connect, play, publish on the first media, feed until
/// it stops.
async fn session(parsed: &ParsedUrl, pull: &RtmpPull, registry: &Arc<Registry>, buffer: BufferConfig) -> Outcome {
    let (mut client, first) = match RtmpClient::connect_play(parsed, CONNECT_TIMEOUT).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(stream = %pull.stream, url = %redact_url(&pull.url), error = %e, "rtmp pull: connect failed");
            return Outcome::Failed;
        }
    };
    let mut ingest: Option<FlvIngest> = None;
    let mut fps = None;
    for ev in first {
        if let Some(o) = handle(ev, pull, registry, buffer, &mut ingest, &mut fps) {
            return o;
        }
    }
    // Idle is measured in media, not bytes: a server that only pings keeps
    // the socket busy without the stream being live.
    let mut last_media = tokio::time::Instant::now();
    loop {
        let wait = (last_media + IDLE).saturating_duration_since(tokio::time::Instant::now());
        let n = match tokio::time::timeout(wait, client.read_some()).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => {
                tracing::info!(stream = %pull.stream, error = %e, "rtmp pull: connection ended");
                break;
            }
            Err(_) => {
                tracing::info!(stream = %pull.stream, idle = ?IDLE, "rtmp pull: no media");
                break;
            }
        };
        let events = match client.process_play(n).await {
            Ok(ev) => ev,
            Err(e) => {
                tracing::info!(stream = %pull.stream, error = %e, "rtmp pull: protocol error");
                break;
            }
        };
        for ev in events {
            if matches!(ev, PlayEvent::Video { .. } | PlayEvent::Audio { .. }) {
                last_media = tokio::time::Instant::now();
            }
            if let Some(o) = handle(ev, pull, registry, buffer, &mut ingest, &mut fps) {
                return o;
            }
        }
    }
    if ingest.is_some() { Outcome::Flowed } else { Outcome::Failed }
}

/// Runs one pull until its task is aborted (a reload removing it, or
/// shutdown).
async fn run(pull: RtmpPull, registry: Arc<Registry>, buffer: BufferConfig) {
    let parsed = match parse_url(&pull.url) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(stream = %pull.stream, error = %e, "rtmp pull: bad url; not started");
            return;
        }
    };
    tracing::info!(stream = %pull.stream, url = %redact_url(&pull.url), "rtmp pull started");
    let mut backoff = BACKOFF_MIN;
    loop {
        match session(&parsed, &pull, &registry, buffer).await {
            Outcome::Flowed => {
                tracing::info!(stream = %pull.stream, "rtmp pull: stream ended; reconnecting");
                backoff = BACKOFF_MIN;
            }
            Outcome::Failed => backoff = (backoff * 2).min(BACKOFF_MAX),
            Outcome::Fatal => return,
        }
        tokio::time::sleep(backoff).await;
    }
}

struct Entry {
    pull: RtmpPull,
    task: tokio::task::JoinHandle<()>,
}

/// The running `[[rtmp.pull]]`s. Cheap to clone.
#[derive(Clone)]
pub struct RtmpPullHandle {
    entries: Arc<Mutex<Vec<Entry>>>,
    buffer: BufferConfig,
}

fn spawn(registry: Arc<Registry>, buffer: BufferConfig, pull: RtmpPull) -> Entry {
    let p = pull.clone();
    Entry { pull, task: tokio::spawn(run(p, registry, buffer)) }
}

/// Starts every pull. Must be called inside a tokio runtime.
pub fn start_pulls(registry: Arc<Registry>, buffer: BufferConfig, pulls: Vec<RtmpPull>) -> RtmpPullHandle {
    let entries = pulls.into_iter().map(|p| spawn(registry.clone(), buffer, p)).collect();
    RtmpPullHandle { entries: Arc::new(Mutex::new(entries)), buffer }
}

impl RtmpPullHandle {
    /// Unchanged pulls keep their connection; removed ones stop (ending
    /// their stream), new or changed ones start.
    pub fn reload(&self, registry: &Arc<Registry>, pulls: Vec<RtmpPull>) {
        let mut entries = self.entries.lock();
        let mut remaining = std::mem::take(&mut *entries);
        let mut next = Vec::with_capacity(pulls.len());
        for p in pulls {
            match remaining.iter().position(|e| e.pull == p) {
                Some(pos) => next.push(remaining.remove(pos)),
                None => next.push(spawn(registry.clone(), self.buffer, p)),
            }
        }
        for gone in remaining {
            gone.task.abort();
        }
        *entries = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert!(parse_url("rtmp://10.0.0.5/live/cam1").is_ok());
        assert!(parse_url("rtmps://live.example:443/app/key").is_ok());
        assert!(parse_url("http://h/live/key").is_err());
        assert!(parse_url("rtmp://h/").is_err(), "needs a key");
        let e = parse_url("rtmp://h:bad/live/super-secret").unwrap_err();
        assert!(!e.contains("super-secret"), "the key is never in an error: {e}");
    }
}
