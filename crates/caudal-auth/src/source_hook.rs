//! Source webhook: when a viewer asks for a stream that is neither live nor
//! configured, ask an external service where to pull it from (MistServer's
//! `STREAM_SOURCE`, `docs/research/MISTSERVER-REVERSE.md` §1.4 and §4.2).
//!
//! Same machinery as [`crate::RewriteHook`] (Standard Webhooks signing, one
//! attempt within `timeout`, a single-flight cache), different question.
//! Wire format: body `{"type": "stream.source", "timestamp", "data":
//! {"stream"}}`. Answer:
//!
//! - 2xx with a JSON object: `source` is the URL to pull; absent, null or
//!   empty means "no such stream". An optional `cache_secs` lowers how long
//!   the answer is remembered (0: not at all).
//! - 2xx with any other body (MistServer's plain-text trigger answer): the
//!   first trimmed line is the URL; empty means "no such stream".
//! - Anything else (401/403 included), a timeout, a connection error: no
//!   source. There is no `on_error = "allow"`: without a URL there is
//!   nothing to start.
//!
//! The URL is not checked here; the caller decides which kinds of source it
//! can pull. Answers are cached per stream name: the source of a stream does
//! not depend on which viewer asks first (the viewers are already through
//! `[auth]` and share the one pull).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::blocking::{FlightCache, Signer, small_body};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceHookConfig {
    pub url: String,
    /// Standard Webhooks secret (`whsec_...`).
    pub secret: String,
    pub timeout: Duration,
    /// How long a source is remembered at most (the service's `cache_secs`
    /// can only lower it).
    pub cache: Duration,
    /// How long "no such stream" or a failure is remembered.
    pub deny_cache: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Answer {
    Source(String, Duration),
    None,
    Failed,
}

const OUTCOMES: [&str; 3] = ["source", "none", "error"];

/// Counters for `/metrics`. Survive reloads: the supervisor owns one and
/// hands it to every new [`SourceHook`].
#[derive(Debug, Default)]
pub struct SourceHookStats {
    calls: [AtomicU64; 3],
    cached: AtomicU64,
    wait_micros: AtomicU64,
}

impl SourceHookStats {
    fn record(&self, a: &Answer, waited: Duration) {
        let o = match a {
            Answer::Source(..) => 0,
            Answer::None => 1,
            Answer::Failed => 2,
        };
        self.calls[o].fetch_add(1, Ordering::Relaxed);
        self.wait_micros.fetch_add(waited.as_micros().min(u64::MAX as u128) as u64, Ordering::Relaxed);
    }

    /// Calls by outcome: `source` (a URL came back), `none` (no such
    /// stream, or a refusal), `error`.
    pub fn calls(&self) -> [(&'static str, u64); 3] {
        std::array::from_fn(|i| (OUTCOMES[i], self.calls[i].load(Ordering::Relaxed)))
    }

    /// Requests answered from the cache.
    pub fn cached(&self) -> u64 {
        self.cached.load(Ordering::Relaxed)
    }

    /// Seconds spent waiting on the service.
    pub fn wait_seconds(&self) -> f64 {
        self.wait_micros.load(Ordering::Relaxed) as f64 / 1e6
    }
}

/// The source webhook. Cheap to share behind an `Arc`.
pub struct SourceHook {
    cfg: SourceHookConfig,
    /// `None`: a hook that could not start and never finds a source.
    client: Option<Signer>,
    cache: FlightCache<String, Answer>,
    stats: Arc<SourceHookStats>,
}

impl SourceHook {
    /// `Err` on a secret Standard Webhooks cannot parse.
    pub fn new(cfg: SourceHookConfig, stats: Arc<SourceHookStats>) -> Result<Self, String> {
        let client = Signer::new("[auth.source]", &cfg.url, &cfg.secret, cfg.timeout)?;
        Ok(Self { cfg, client: Some(client), cache: FlightCache::default(), stats })
    }

    /// A hook that finds no source for anything: what the server installs
    /// when a configured hook cannot start.
    pub fn refusing(stats: Arc<SourceHookStats>) -> Self {
        let cfg = SourceHookConfig {
            url: String::new(),
            secret: String::new(),
            timeout: Duration::from_millis(100),
            cache: Duration::ZERO,
            deny_cache: Duration::ZERO,
        };
        Self { cfg, client: None, cache: FlightCache::default(), stats }
    }

    /// The source URL for `stream`, or `None` for "no such stream" (and for
    /// a service that failed).
    pub async fn source(&self, stream: &str) -> Option<String> {
        self.client.as_ref()?;
        let (cell, from_cache) = self.cache.cell(stream.to_owned(), |a| match a {
            Answer::Source(_, ttl) => *ttl,
            _ => self.cfg.deny_cache,
        });
        if from_cache {
            self.stats.cached.fetch_add(1, Ordering::Relaxed);
        }
        let answer = cell
            .get_or_init(|| async {
                let started = Instant::now();
                let a = self.call(stream).await;
                self.stats.record(&a, started.elapsed());
                a
            })
            .await;
        match answer {
            Answer::Source(url, _) => Some(url.clone()),
            _ => None,
        }
    }

    async fn call(&self, stream: &str) -> Answer {
        let failed = |why: &str| {
            tracing::warn!(stream, why, "source hook failed; the stream is not available");
            Answer::Failed
        };
        let Some(client) = &self.client else { return failed("not started") };
        let resp = match client.post("stream.source", json!({ "stream": stream })).await {
            Ok(r) => r,
            Err(e) => return failed(e.as_str()),
        };
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            tracing::info!(stream, %status, "source hook refused: no such stream");
            return Answer::None;
        }
        if !status.is_success() {
            return failed(status.as_str());
        }
        let Some(body) = small_body(resp, self.cfg.timeout).await else { return failed("body") };
        let Some((url, hint)) = parse_answer(&body) else { return failed("body") };
        if url.is_empty() {
            tracing::info!(stream, "source hook answered no source");
            return Answer::None;
        }
        let ttl = hint.map_or(self.cfg.cache, |h| h.min(self.cfg.cache));
        Answer::Source(url, ttl)
    }
}

/// `(url, cache hint)` from a 2xx body: a JSON object's `source` and
/// `cache_secs`, or a plain-text URL. `None` for a body that is neither.
fn parse_answer(body: &[u8]) -> Option<(String, Option<Duration>)> {
    let text = std::str::from_utf8(body).ok()?.trim();
    if text.starts_with('{') {
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        let url = match v.get("source") {
            None | Some(serde_json::Value::Null) => String::new(),
            Some(serde_json::Value::String(s)) => s.trim().to_owned(),
            Some(_) => return None,
        };
        let hint = v.get("cache_secs").and_then(serde_json::Value::as_u64).map(Duration::from_secs);
        return Some((url, hint));
    }
    Some((text.lines().next().unwrap_or("").trim().to_owned(), None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers() {
        assert_eq!(parse_answer(b"srt://10.0.0.5:9000\n"), Some(("srt://10.0.0.5:9000".into(), None)));
        assert_eq!(parse_answer(b""), Some((String::new(), None)));
        assert_eq!(
            parse_answer(br#"{"source":"http://a/b.ts","cache_secs":0}"#),
            Some(("http://a/b.ts".into(), Some(Duration::ZERO)))
        );
        assert_eq!(parse_answer(br#"{"source":null}"#), Some((String::new(), None)));
        assert_eq!(parse_answer(br#"{}"#), Some((String::new(), None)));
        assert_eq!(parse_answer(br#"{"source":5}"#), None);
        assert_eq!(parse_answer(&[0xff]), None);
    }
}
