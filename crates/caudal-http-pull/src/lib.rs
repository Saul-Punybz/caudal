//! `[[http.pull]]`: MPEG-TS served over HTTP(S) into Caudal. VLC's
//! `#standard{access=http,mux=ts,dst=...}`, ffmpeg's `-listen 1
//! http://...`, an IRD or encoder with an HTTP output, and MistServer's
//! `http-ts://` inputs all serve a transport stream as one endless HTTP
//! response body. Caudal sends a GET, feeds the body to a
//! [`caudal_ts::ingest::TsIngest`] (the demuxer SRT and UDP use, so every
//! codec they take works here too) and publishes it as `stream`.
//!
//! - The stream is published when the first bytes arrive, so a source that
//!   is down leaves no empty stream behind.
//! - [`IDLE`] without bytes, or the response ending, ends the stream; the
//!   pull then reconnects with backoff ([`BACKOFF_MIN`]..[`BACKOFF_MAX`],
//!   back to the minimum once data flowed).
//! - An HLS playlist (`.m3u8`) is not a TS body: [`parse_url`] refuses it
//!   and points at `[[hls.pull]]` (crate `caudal-hls-pull`).

use std::sync::Arc;
use std::time::Duration;

use caudal_core::{BufferConfig, PublishError, Registry};
use caudal_ts::ingest::TsIngest;
use parking_lot::Mutex;

/// A live TS that sends nothing for this long has stopped.
pub const IDLE: Duration = Duration::from_secs(5);
pub const BACKOFF_MIN: Duration = Duration::from_secs(1);
pub const BACKOFF_MAX: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// One `[[http.pull]]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpPull {
    pub stream: String,
    pub url: String,
}

/// Checks a pull URL: `http://` or `https://` with a host, and not an HLS
/// playlist.
pub fn parse_url(url: &str) -> Result<reqwest::Url, String> {
    let shown = caudal_core::net::redact_url(url);
    let u = reqwest::Url::parse(url).map_err(|e| format!("{shown:?}: {e}"))?;
    if !matches!(u.scheme(), "http" | "https") {
        return Err(format!("{shown:?}: expected http:// or https://"));
    }
    if u.host_str().is_none_or(str::is_empty) {
        return Err(format!("{shown:?}: needs a host"));
    }
    if u.path().to_ascii_lowercase().ends_with(".m3u8") {
        return Err(format!(
            "{shown:?}: an HLS playlist, not an MPEG-TS stream; [[http.pull]] takes a TS served over HTTP \
             (VLC access=http,mux=ts): pull a playlist with [[hls.pull]]"
        ));
    }
    Ok(u)
}

enum Outcome {
    /// Bytes flowed, then stopped (idle, the body ended, an error).
    Flowed,
    /// Never got a byte: the connect, the status or the first read failed.
    Failed,
    /// Nothing will ever work (bad name): stop the pull.
    Fatal,
}

/// One connection: GET, publish on the first bytes, feed until it stops.
async fn session(client: &reqwest::Client, pull: &HttpPull, registry: &Arc<Registry>, buffer: BufferConfig) -> Outcome {
    let mut resp = match client.get(&pull.url).send().await {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            tracing::warn!(stream = %pull.stream, url = %caudal_core::net::redact_url(&pull.url), status = %r.status(), "http pull: not served");
            return Outcome::Failed;
        }
        Err(e) => {
            tracing::warn!(stream = %pull.stream, url = %caudal_core::net::redact_url(&pull.url), error = %e.without_url(), "http pull: connect failed");
            return Outcome::Failed;
        }
    };
    let mut ingest: Option<TsIngest> = None;
    loop {
        match tokio::time::timeout(IDLE, resp.chunk()).await {
            Ok(Ok(Some(bytes))) => {
                if ingest.is_none() {
                    match registry.publish(&pull.stream, buffer) {
                        Ok(p) => {
                            tracing::info!(stream = %pull.stream, url = %caudal_core::net::redact_url(&pull.url), "http pull: data arriving; published");
                            ingest = Some(TsIngest::new(p));
                        }
                        Err(PublishError::Busy(_)) => {
                            tracing::warn!(stream = %pull.stream, "http pull: stream already has a publisher; retrying");
                            return Outcome::Failed;
                        }
                        Err(PublishError::InvalidName) => {
                            tracing::error!(stream = %pull.stream, "http pull: invalid stream name; stopped");
                            return Outcome::Fatal;
                        }
                    }
                }
                if let Some(i) = ingest.as_mut() {
                    i.feed(&bytes);
                }
            }
            Ok(Ok(None)) => {
                tracing::info!(stream = %pull.stream, url = %caudal_core::net::redact_url(&pull.url), "http pull: the source closed the response");
                break;
            }
            Ok(Err(e)) => {
                tracing::info!(stream = %pull.stream, url = %caudal_core::net::redact_url(&pull.url), error = %e.without_url(), "http pull: read failed");
                break;
            }
            Err(_) => {
                tracing::info!(stream = %pull.stream, idle = ?IDLE, "http pull: no data");
                break;
            }
        }
    }
    if ingest.is_some() { Outcome::Flowed } else { Outcome::Failed }
}

/// Runs one pull until its task is aborted (a reload removing it, or
/// shutdown).
async fn run(pull: HttpPull, registry: Arc<Registry>, buffer: BufferConfig) {
    if let Err(e) = parse_url(&pull.url) {
        tracing::error!(stream = %pull.stream, error = %e, "http pull: bad url; not started");
        return;
    }
    let client = match reqwest::Client::builder().connect_timeout(CONNECT_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(stream = %pull.stream, error = %e, "http pull: no HTTP client; not started");
            return;
        }
    };
    tracing::info!(stream = %pull.stream, url = %caudal_core::net::redact_url(&pull.url), "http pull started");
    let mut backoff = BACKOFF_MIN;
    loop {
        match session(&client, &pull, &registry, buffer).await {
            Outcome::Flowed => {
                tracing::info!(stream = %pull.stream, "http pull: stream ended; reconnecting");
                backoff = BACKOFF_MIN;
            }
            Outcome::Failed => backoff = (backoff * 2).min(BACKOFF_MAX),
            Outcome::Fatal => return,
        }
        tokio::time::sleep(backoff).await;
    }
}

struct Entry {
    pull: HttpPull,
    task: tokio::task::JoinHandle<()>,
}

/// The running `[[http.pull]]`s. Cheap to clone.
#[derive(Clone)]
pub struct HttpPullHandle {
    entries: Arc<Mutex<Vec<Entry>>>,
    buffer: BufferConfig,
}

fn spawn(registry: Arc<Registry>, buffer: BufferConfig, pull: HttpPull) -> Entry {
    let p = pull.clone();
    Entry { pull, task: tokio::spawn(run(p, registry, buffer)) }
}

/// Starts every pull. Must be called inside a tokio runtime.
pub fn start_pulls(registry: Arc<Registry>, buffer: BufferConfig, pulls: Vec<HttpPull>) -> HttpPullHandle {
    let entries = pulls.into_iter().map(|p| spawn(registry.clone(), buffer, p)).collect();
    HttpPullHandle { entries: Arc::new(Mutex::new(entries)), buffer }
}

impl HttpPullHandle {
    /// Unchanged pulls keep their connection; removed ones stop (ending
    /// their stream), new or changed ones start.
    pub fn reload(&self, registry: &Arc<Registry>, pulls: Vec<HttpPull>) {
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
        assert!(parse_url("http://10.0.0.5:8080/stream.ts").is_ok());
        assert!(parse_url("https://encoder.example/live").is_ok());
        assert!(parse_url("http://10.0.0.5:8080/").is_ok(), "VLC serves at any path");
        assert!(parse_url("udp://10.0.0.5:5000").unwrap_err().contains("http://"));
        assert!(parse_url("http://h/live/index.m3u8").unwrap_err().contains("HLS"));
        assert!(parse_url("http://h/LIVE.M3U8?token=x").unwrap_err().contains("HLS"));
        assert!(parse_url("not a url").is_err());
        let e = parse_url("https://u:pw@cdn.example/live/x.m3u8?token=s3cret").unwrap_err();
        assert!(!e.contains("s3cret") && !e.contains(":pw@"), "{e}");
    }
}
