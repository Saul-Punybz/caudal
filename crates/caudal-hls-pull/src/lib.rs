//! `[[hls.pull]]`: a remote HLS or LL-HLS stream into Caudal, what
//! MistServer's HLS input does. Caudal fetches the playlist (`.m3u8`), picks
//! the highest-`BANDWIDTH` variant of a master playlist, polls the media
//! playlist and fetches each new segment once, in media-sequence order,
//! feeding it to one long-lived [`caudal_ts::ingest::TsIngest`] (the
//! demuxer SRT, UDP and HTTP-TS use, so the same codecs work).
//!
//! - Live: starts [`LIVE_EDGE_SEGMENTS`] segments from the end, as players
//!   do, then polls every target duration (half of one when nothing was
//!   new, as RFC 8216 §6.3.4 asks). LL-HLS playlists work the same way, on
//!   whole segments: parts and blocking reloads are not used, so the
//!   latency is that of plain HLS.
//! - Segments are fed at most a few target durations ahead of real time,
//!   so a playlist with `EXT-X-ENDLIST` from the start (VOD) plays out at
//!   its own pace instead of all at once.
//! - A segment that fails to download is skipped (logged); segments that
//!   slid out of the playlist before they were fetched are a gap.
//!   `EXT-X-DISCONTINUITY`, and a media sequence that starts over (the
//!   source restarted), splice the timeline with
//!   [`TsIngest::discontinuity`].
//! - The stream is published on the first segment and ends after
//!   [`STALL_TARGET_DURATIONS`] target durations without a new one, or
//!   when the last segment of an `EXT-X-ENDLIST` playlist was fed. After an
//!   `EXT-X-ENDLIST` the pull waits for the playlist to go live again
//!   rather than replaying it. Failures reconnect with backoff
//!   ([`BACKOFF_MIN`]..[`BACKOFF_MAX`]).
//! - MPEG-TS segments only: fMP4/CMAF (`EXT-X-MAP`), encrypted
//!   (`EXT-X-KEY`) and packed-audio segments are refused with a log line.

pub mod playlist;

use std::sync::Arc;
use std::time::Duration;

use caudal_core::{BufferConfig, PublishError, Registry};
use caudal_ts::ingest::TsIngest;
use parking_lot::Mutex;
use reqwest::Url;
use tokio::time::Instant;

use crate::playlist::{Media, Playlist};

pub const BACKOFF_MIN: Duration = Duration::from_secs(1);
pub const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// A live stream starts this many segments before the newest one.
pub const LIVE_EDGE_SEGMENTS: usize = 3;
/// No new segment for this many target durations: the source stopped.
pub const STALL_TARGET_DURATIONS: u32 = 3;
/// The stall timeout is never shorter than this.
pub const STALL_MIN: Duration = Duration::from_secs(4);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const TS_SYNC: u8 = 0x47;

/// One `[[hls.pull]]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsPull {
    pub stream: String,
    pub url: String,
}

/// Checks a pull URL: `http://` or `https://` with a host. The path need
/// not end in `.m3u8`; what the server sends is checked when fetched.
pub fn parse_url(url: &str) -> Result<Url, String> {
    let u = Url::parse(url).map_err(|e| format!("{url:?}: {e}"))?;
    if !matches!(u.scheme(), "http" | "https") {
        return Err(format!("{url:?}: expected http:// or https://"));
    }
    if u.host_str().is_none_or(str::is_empty) {
        return Err(format!("{url:?}: needs a host"));
    }
    Ok(u)
}

/// True when `url` names an HLS playlist (its path ends in `.m3u8`).
pub fn is_playlist_url(url: &str) -> bool {
    Url::parse(url).is_ok_and(|u| u.path().to_ascii_lowercase().ends_with(".m3u8"))
}

enum Outcome {
    /// Segments were published, then stopped coming.
    Flowed,
    /// The `EXT-X-ENDLIST` playlist was published to its end.
    Ended,
    /// Nothing was published: the playlist or segments could not be had.
    Failed,
    /// The source is something this pull cannot play (fMP4, encrypted).
    Unsupported,
    /// The playlist still ends with `EXT-X-ENDLIST`: nothing new to play.
    StillEnded,
    /// Nothing will ever work (bad name): stop the pull.
    Fatal,
}

async fn get(client: &reqwest::Client, url: &Url) -> Result<reqwest::Response, String> {
    match client.get(url.clone()).send().await {
        Ok(r) if r.status().is_success() => Ok(r),
        Ok(r) => Err(format!("{url}: HTTP {}", r.status())),
        Err(e) => Err(format!("{url}: {e}")),
    }
}

/// Fetches and parses a playlist; the base for its URIs is where it was
/// served from after redirects.
async fn fetch_playlist(client: &reqwest::Client, url: &Url) -> Result<(Url, Playlist), String> {
    let resp = get(client, url).await?;
    let base = resp.url().clone();
    let text = resp.text().await.map_err(|e| format!("{url}: {e}"))?;
    let p = playlist::parse(&text, &base).map_err(|e| format!("{base}: {e}"))?;
    Ok((base, p))
}

/// The media playlist behind `url`: itself, or the best variant of a
/// master playlist.
async fn resolve(client: &reqwest::Client, url: &Url) -> Result<(Url, Media), String> {
    match fetch_playlist(client, url).await? {
        (base, Playlist::Media(m)) => Ok((base, m)),
        (_, Playlist::Master(variants)) => {
            let best = playlist::best_variant(&variants).expect("a master playlist has variants");
            tracing::debug!(variant = %best.uri, bandwidth = best.bandwidth, "hls pull: picked the highest bandwidth");
            match fetch_playlist(client, &best.uri).await? {
                (base, Playlist::Media(m)) => Ok((base, m)),
                (base, Playlist::Master(_)) => Err(format!("{base}: a master playlist inside a master playlist")),
            }
        }
    }
}

/// Why a media playlist cannot be pulled, if it cannot.
fn unsupported(m: &Media) -> Option<&'static str> {
    if m.map.is_some() {
        Some("fMP4/CMAF segments (EXT-X-MAP) are not supported yet; only MPEG-TS segments")
    } else if m.encrypted {
        Some("encrypted segments (EXT-X-KEY) are not supported")
    } else {
        None
    }
}

fn stall_after(target_duration: f64) -> Duration {
    Duration::from_secs_f64(target_duration * f64::from(STALL_TARGET_DURATIONS)).max(STALL_MIN)
}

/// The first segment a new session plays.
fn start_seq(m: &Media) -> u64 {
    if m.endlist {
        m.segments.first().map_or(0, |s| s.seq)
    } else {
        let i = m.segments.len().saturating_sub(LIVE_EDGE_SEGMENTS);
        m.segments.get(i).map_or(0, |s| s.seq)
    }
}

/// One pull session: resolve, then feed new segments until the source
/// stops. `after_end`: the last session ended on `EXT-X-ENDLIST`.
async fn session(
    client: &reqwest::Client,
    pull: &HlsPull,
    url: &Url,
    registry: &Arc<Registry>,
    buffer: BufferConfig,
    after_end: bool,
) -> Outcome {
    let (media_url, mut media) = match resolve(client, url).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(stream = %pull.stream, error = %e, "hls pull: no playlist");
            return Outcome::Failed;
        }
    };
    if after_end && media.endlist {
        return Outcome::StillEnded;
    }
    let mut next_seq = start_seq(&media);
    let mut ingest: Option<TsIngest> = None;
    let mut splice = false;
    let mut last_new = Instant::now();
    // Pacing: media seconds fed since the first segment, against the clock.
    let mut fed_secs = 0.0f64;
    let mut started: Option<Instant> = None;
    loop {
        if let Some(why) = unsupported(&media) {
            tracing::error!(stream = %pull.stream, url = %media_url, "hls pull: {why}");
            return Outcome::Unsupported;
        }
        let td = media.target_duration;
        let first = media.segments.first().map(|s| s.seq);
        let last = media.segments.last().map(|s| s.seq);
        match (first, last) {
            (_, Some(last)) if last.saturating_add(1) < next_seq => {
                tracing::info!(stream = %pull.stream, next_seq, last, "hls pull: media sequence started over; splicing");
                next_seq = start_seq(&media);
                splice = true;
            }
            (Some(first), _) if first > next_seq && ingest.is_some() => {
                tracing::warn!(
                    stream = %pull.stream,
                    missed = first - next_seq,
                    "hls pull: segments left the playlist before they were fetched"
                );
                next_seq = first;
            }
            _ => {}
        }
        let mut fresh = false;
        let from = next_seq;
        for seg in media.segments.iter().filter(|s| s.seq >= from) {
            if let Some(t0) = started {
                let ahead = fed_secs - t0.elapsed().as_secs_f64() - td * f64::from(STALL_TARGET_DURATIONS - 1);
                if ahead > 0.0 {
                    tokio::time::sleep(Duration::from_secs_f64(ahead)).await;
                }
            }
            next_seq = seg.seq + 1;
            fresh = true;
            last_new = Instant::now();
            let body = match get(client, &seg.uri).await {
                Ok(r) => r.bytes().await.map_err(|e| format!("{}: {e}", seg.uri)),
                Err(e) => Err(e),
            };
            let body = match body {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(stream = %pull.stream, seq = seg.seq, error = %e, "hls pull: segment missed");
                    continue;
                }
            };
            if body.first() != Some(&TS_SYNC) {
                tracing::error!(
                    stream = %pull.stream,
                    segment = %seg.uri,
                    "hls pull: the segment is not MPEG-TS (fMP4 without EXT-X-MAP, or packed audio); not supported"
                );
                return if ingest.is_some() { Outcome::Flowed } else { Outcome::Unsupported };
            }
            if ingest.is_none() {
                match registry.publish(&pull.stream, buffer) {
                    Ok(p) => {
                        tracing::info!(stream = %pull.stream, url = %media_url, "hls pull: segments arriving; published");
                        ingest = Some(TsIngest::new(p));
                        started = Some(Instant::now());
                    }
                    Err(PublishError::Busy(_)) => {
                        tracing::warn!(stream = %pull.stream, "hls pull: stream already has a publisher; retrying");
                        return Outcome::Failed;
                    }
                    Err(PublishError::InvalidName) => {
                        tracing::error!(stream = %pull.stream, "hls pull: invalid stream name; stopped");
                        return Outcome::Fatal;
                    }
                }
            } else if (seg.discontinuity || splice)
                && let Some(i) = ingest.as_mut()
            {
                i.discontinuity();
            }
            splice = false;
            fed_secs += seg.duration;
            if let Some(i) = ingest.as_mut() {
                i.feed(&body);
            }
        }
        if media.endlist && media.segments.last().is_none_or(|s| s.seq < next_seq) {
            tracing::info!(stream = %pull.stream, "hls pull: EXT-X-ENDLIST reached; stream ended");
            return if ingest.is_some() { Outcome::Ended } else { Outcome::StillEnded };
        }
        if last_new.elapsed() >= stall_after(td) {
            tracing::info!(stream = %pull.stream, "hls pull: no new segments");
            return if ingest.is_some() { Outcome::Flowed } else { Outcome::Failed };
        }
        let wait = if fresh { td } else { td / 2.0 };
        tokio::time::sleep(Duration::from_secs_f64(wait.max(0.1))).await;
        match fetch_playlist(client, &media_url).await {
            Ok((_, Playlist::Media(m))) => media = m,
            Ok((_, Playlist::Master(_))) => {
                tracing::warn!(stream = %pull.stream, url = %media_url, "hls pull: the media playlist became a master");
            }
            Err(e) => tracing::warn!(stream = %pull.stream, error = %e, "hls pull: playlist reload failed"),
        }
    }
}

/// Runs one pull until its task is aborted (a reload removing it, or
/// shutdown).
async fn run(pull: HlsPull, registry: Arc<Registry>, buffer: BufferConfig) {
    let url = match parse_url(&pull.url) {
        Ok(u) => u,
        Err(e) => {
            tracing::error!(stream = %pull.stream, error = %e, "hls pull: bad url; not started");
            return;
        }
    };
    let client = match reqwest::Client::builder().connect_timeout(CONNECT_TIMEOUT).timeout(REQUEST_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(stream = %pull.stream, error = %e, "hls pull: no HTTP client; not started");
            return;
        }
    };
    tracing::info!(stream = %pull.stream, url = %pull.url, "hls pull started");
    let mut backoff = BACKOFF_MIN;
    let mut after_end = false;
    loop {
        match session(&client, &pull, &url, &registry, buffer, after_end).await {
            Outcome::Flowed => {
                tracing::info!(stream = %pull.stream, "hls pull: stream ended; reconnecting");
                backoff = BACKOFF_MIN;
                after_end = false;
            }
            Outcome::Ended => {
                backoff = BACKOFF_MIN;
                after_end = true;
            }
            Outcome::StillEnded => {
                backoff = (backoff * 2).min(BACKOFF_MAX);
                after_end = true;
            }
            Outcome::Failed => backoff = (backoff * 2).min(BACKOFF_MAX),
            Outcome::Unsupported => backoff = BACKOFF_MAX,
            Outcome::Fatal => return,
        }
        tokio::time::sleep(backoff).await;
    }
}

struct Entry {
    pull: HlsPull,
    task: tokio::task::JoinHandle<()>,
}

/// The running `[[hls.pull]]`s. Cheap to clone.
#[derive(Clone)]
pub struct HlsPullHandle {
    entries: Arc<Mutex<Vec<Entry>>>,
    buffer: BufferConfig,
}

fn spawn(registry: Arc<Registry>, buffer: BufferConfig, pull: HlsPull) -> Entry {
    let p = pull.clone();
    Entry { pull, task: tokio::spawn(run(p, registry, buffer)) }
}

/// Starts every pull. Must be called inside a tokio runtime.
pub fn start_pulls(registry: Arc<Registry>, buffer: BufferConfig, pulls: Vec<HlsPull>) -> HlsPullHandle {
    let entries = pulls.into_iter().map(|p| spawn(registry.clone(), buffer, p)).collect();
    HlsPullHandle { entries: Arc::new(Mutex::new(entries)), buffer }
}

impl HlsPullHandle {
    /// Unchanged pulls keep running; removed ones stop (ending their
    /// stream), new or changed ones start.
    pub fn reload(&self, registry: &Arc<Registry>, pulls: Vec<HlsPull>) {
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
        assert!(parse_url("http://10.0.0.5:8080/live/index.m3u8").is_ok());
        assert!(parse_url("https://cdn.example/master.m3u8?token=x").is_ok());
        assert!(parse_url("https://cdn.example/playlist").is_ok(), "any path");
        assert!(parse_url("rtmp://h/app").unwrap_err().contains("http://"));
        assert!(parse_url("not a url").is_err());
        assert!(is_playlist_url("http://h/live/INDEX.M3U8?token=x"));
        assert!(!is_playlist_url("http://h/stream.ts"));
        assert!(!is_playlist_url("http://h/?u=a.m3u8"), "the path, not the query");
    }

    fn media(endlist: bool, seqs: std::ops::Range<u64>) -> Media {
        let segments = seqs
            .map(|seq| playlist::Segment {
                seq,
                uri: Url::parse(&format!("http://h/{seq}.ts")).unwrap(),
                duration: 2.0,
                discontinuity: false,
            })
            .collect();
        Media { target_duration: 2.0, segments, endlist, map: None, encrypted: false }
    }

    #[test]
    fn where_a_session_starts() {
        assert_eq!(start_seq(&media(false, 10..16)), 13, "three from the live edge");
        assert_eq!(start_seq(&media(false, 10..12)), 10, "a short playlist from its start");
        assert_eq!(start_seq(&media(true, 10..16)), 10, "VOD from the start");
        assert_eq!(start_seq(&media(false, 0..0)), 0);
        assert_eq!(stall_after(1.0), STALL_MIN);
        assert_eq!(stall_after(6.0), Duration::from_secs(18));
    }

    #[test]
    fn refused_sources() {
        let mut m = media(false, 0..3);
        assert!(unsupported(&m).is_none());
        m.map = Some(Url::parse("http://h/init.mp4").unwrap());
        assert!(unsupported(&m).unwrap().contains("fMP4"));
        m.map = None;
        m.encrypted = true;
        assert!(unsupported(&m).unwrap().contains("encrypted"));
    }
}
