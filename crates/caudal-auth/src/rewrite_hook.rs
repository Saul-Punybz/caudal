//! Blocking routing webhook: before a publish or a play, ask an external
//! service which stream the request really goes to (MistServer's
//! `PUSH_REWRITE` and `PLAY_REWRITE`, `docs/research/MISTSERVER-REVERSE.md`
//! §4.2).
//!
//! Same machinery as [`crate::GateHook`] (Standard Webhooks signing, one
//! attempt within `timeout`, `on_error`, a single-flight cache), different
//! question. Wire format: body `{"type": "rewrite.publish" |
//! "rewrite.play", "timestamp", "data": {"stream", "protocol", "ip",
//! "token"}}`. Answer:
//!
//! - 2xx with a JSON object: `stream` is the name to use; absent, null or
//!   empty denies. An optional `cache_secs` lowers how long this answer is
//!   remembered (0: not at all).
//! - 2xx with any other body (MistServer's plain-text trigger answer): the
//!   trimmed body is the name; empty denies.
//! - 401/403: denies (an optional `{"reason": "..."}` is logged).
//! - Anything else, a timeout, a connection error: `on_error` decides;
//!   `allow` keeps the requested name.
//!
//! Answers are cached per (action, protocol, stream, ip, token), so two
//! viewers asking for the same name can be sent to different streams.

use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::blocking::{FlightCache, Signer, deny_reason, small_body};
use crate::{Action, OnError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewriteHookConfig {
    pub url: String,
    /// Standard Webhooks secret (`whsec_...`).
    pub secret: String,
    pub timeout: Duration,
    pub on_error: OnError,
    /// Ask on publish.
    pub publish: bool,
    /// Ask on play.
    pub play: bool,
    /// How long an answer is remembered at most (the service's
    /// `cache_secs` can only lower it).
    pub cache: Duration,
    /// How long a denial or a failure is remembered.
    pub deny_cache: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Answer {
    /// Go on under this name, for this long.
    To(String, Duration),
    Deny(String),
    /// The service failed; `on_error` already applied (`true` = keep the
    /// requested name).
    Failed(bool),
}

const OUTCOMES: [&str; 5] = ["rewrite", "keep", "deny", "error_allowed", "error_denied"];
const ACTIONS: [&str; 2] = ["publish", "play"];

fn action_index(a: Action) -> usize {
    match a {
        Action::Publish => 0,
        Action::Play => 1,
    }
}

/// Counters for `/metrics`. Survive reloads: the supervisor owns one and
/// hands it to every new [`RewriteHook`].
#[derive(Debug, Default)]
pub struct RewriteHookStats {
    calls: [[AtomicU64; 5]; 2],
    cached: [AtomicU64; 2],
    wait_micros: [AtomicU64; 2],
}

/// One row of [`RewriteHookStats::calls`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewriteHookMetric {
    pub action: &'static str,
    pub outcome: &'static str,
    pub count: u64,
}

impl RewriteHookStats {
    fn record(&self, action: Action, requested: &str, a: &Answer, waited: Duration) {
        let o = match a {
            Answer::To(name, _) if name != requested => 0,
            Answer::To(..) => 1,
            Answer::Deny(_) => 2,
            Answer::Failed(true) => 3,
            Answer::Failed(false) => 4,
        };
        let i = action_index(action);
        self.calls[i][o].fetch_add(1, Ordering::Relaxed);
        self.wait_micros[i].fetch_add(waited.as_micros().min(u64::MAX as u128) as u64, Ordering::Relaxed);
    }

    /// Calls by action and outcome (`rewrite`: a different name came back,
    /// `keep`: the same name, `deny`, `error_allowed`, `error_denied`).
    pub fn calls(&self) -> Vec<RewriteHookMetric> {
        let mut v = Vec::new();
        for (a, action) in ACTIONS.iter().enumerate() {
            for (o, outcome) in OUTCOMES.iter().enumerate() {
                v.push(RewriteHookMetric { action, outcome, count: self.calls[a][o].load(Ordering::Relaxed) });
            }
        }
        v
    }

    /// Requests answered from the cache, by action.
    pub fn cached(&self) -> [(&'static str, u64); 2] {
        [("publish", self.cached[0].load(Ordering::Relaxed)), ("play", self.cached[1].load(Ordering::Relaxed))]
    }

    /// Seconds spent waiting on the service, by action.
    pub fn wait_seconds(&self) -> [(&'static str, f64); 2] {
        let s = |i: usize| self.wait_micros[i].load(Ordering::Relaxed) as f64 / 1e6;
        [("publish", s(0)), ("play", s(1))]
    }
}

type Key = (Action, &'static str, String, Option<IpAddr>, Option<String>);

/// The routing webhook. Cheap to share behind an `Arc`.
pub struct RewriteHook {
    cfg: RewriteHookConfig,
    /// `None`: a hook that could not start and refuses everything.
    client: Option<Signer>,
    cache: FlightCache<Key, Answer>,
    stats: Arc<RewriteHookStats>,
}

impl RewriteHook {
    /// `Err` on a secret Standard Webhooks cannot parse.
    pub fn new(cfg: RewriteHookConfig, stats: Arc<RewriteHookStats>) -> Result<Self, String> {
        let client = Signer::new("[auth.rewrite]", &cfg.url, &cfg.secret, cfg.timeout)?;
        Ok(Self { cfg, client: Some(client), cache: FlightCache::default(), stats })
    }

    /// A hook that refuses every publish and play it would have routed:
    /// what the server installs when a configured hook cannot start.
    pub fn refusing(publish: bool, play: bool, stats: Arc<RewriteHookStats>) -> Self {
        let cfg = RewriteHookConfig {
            url: String::new(),
            secret: String::new(),
            timeout: Duration::from_millis(100),
            on_error: OnError::Deny,
            publish,
            play,
            cache: Duration::ZERO,
            deny_cache: Duration::ZERO,
        };
        Self { cfg, client: None, cache: FlightCache::default(), stats }
    }

    fn asks(&self, action: Action) -> bool {
        match action {
            Action::Publish => self.cfg.publish,
            Action::Play => self.cfg.play,
        }
    }

    fn ttl(&self, a: &Answer) -> Duration {
        match a {
            Answer::To(_, ttl) => *ttl,
            _ => self.cfg.deny_cache,
        }
    }

    /// `Ok(name)` to go on under `name`; `Err(reason)` to refuse.
    /// `protocol` is sent to the service and keys the cache.
    pub async fn rewrite(
        &self,
        action: Action,
        protocol: &'static str,
        stream: &str,
        token: Option<&str>,
        ip: Option<IpAddr>,
    ) -> Result<String, String> {
        if !self.asks(action) {
            return Ok(stream.to_owned());
        }
        if self.client.is_none() {
            return Err("routing service unavailable".into());
        }
        let key: Key = (action, protocol, stream.to_owned(), ip, token.map(str::to_owned));
        let (cell, from_cache) = self.cache.cell(key, |a| self.ttl(a));
        if from_cache {
            self.stats.cached[action_index(action)].fetch_add(1, Ordering::Relaxed);
        }
        let answer = cell.get_or_init(|| self.ask(action, protocol, stream, token, ip)).await;
        match answer {
            Answer::To(name, _) => Ok(name.clone()),
            Answer::Failed(true) => Ok(stream.to_owned()),
            Answer::Deny(reason) => Err(reason.clone()),
            Answer::Failed(false) => Err("routing service unavailable".into()),
        }
    }

    async fn ask(
        &self,
        action: Action,
        protocol: &'static str,
        stream: &str,
        token: Option<&str>,
        ip: Option<IpAddr>,
    ) -> Answer {
        let started = Instant::now();
        let a = self.call(action, protocol, stream, token, ip).await;
        self.stats.record(action, stream, &a, started.elapsed());
        a
    }

    async fn call(
        &self,
        action: Action,
        protocol: &'static str,
        stream: &str,
        token: Option<&str>,
        ip: Option<IpAddr>,
    ) -> Answer {
        let failed = |why: &str| {
            let allowed = self.cfg.on_error == OnError::Allow;
            tracing::warn!(
                stream,
                protocol,
                action = action.as_str(),
                why,
                allowed,
                "rewrite hook failed; on_error = {}",
                if allowed { "allow (keep the name)" } else { "deny" },
            );
            Answer::Failed(allowed)
        };
        let Some(client) = &self.client else { return failed("not started") };
        let data = json!({
            "stream": stream,
            "protocol": protocol,
            "ip": ip.map(|i| i.to_string()),
            "token": token,
        });
        let resp = match client.post(&format!("rewrite.{}", action.as_str()), data).await {
            Ok(r) => r,
            Err(e) => return failed(e.as_str()),
        };
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            let reason =
                deny_reason(resp, self.cfg.timeout).await.unwrap_or_else(|| "refused by the routing service".into());
            tracing::info!(stream, protocol, action = action.as_str(), %status, reason, "rewrite hook denied");
            return Answer::Deny(reason);
        }
        if !status.is_success() {
            return failed(status.as_str());
        }
        let Some(body) = small_body(resp, self.cfg.timeout).await else { return failed("body") };
        let (name, hint) = match parse_answer(&body) {
            Some(v) => v,
            None => return failed("body"),
        };
        if name.is_empty() {
            tracing::info!(stream, protocol, action = action.as_str(), "rewrite hook answered no stream: denied");
            return Answer::Deny("refused by the routing service".into());
        }
        let ttl = hint.map_or(self.cfg.cache, |h| h.min(self.cfg.cache));
        Answer::To(name, ttl)
    }
}

/// `(name, cache hint)` from a 2xx body: a JSON object's `stream` and
/// `cache_secs`, or a plain-text name. `None` for a body that is neither
/// (not UTF-8, a JSON value other than an object, a non-string `stream`).
fn parse_answer(body: &[u8]) -> Option<(String, Option<Duration>)> {
    let text = std::str::from_utf8(body).ok()?.trim();
    if text.starts_with('{') {
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        let name = match v.get("stream") {
            None | Some(serde_json::Value::Null) => String::new(),
            Some(serde_json::Value::String(s)) => s.trim().to_owned(),
            Some(_) => return None,
        };
        let hint = v.get("cache_secs").and_then(serde_json::Value::as_u64).map(Duration::from_secs);
        return Some((name, hint));
    }
    // MistServer's triggers answer with the bare name; only the first line
    // counts.
    Some((text.lines().next().unwrap_or("").trim().to_owned(), None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers() {
        assert_eq!(parse_answer(b"real\n"), Some(("real".into(), None)));
        assert_eq!(parse_answer(b""), Some((String::new(), None)));
        assert_eq!(parse_answer(br#"{"stream":"real","cache_secs":0}"#), Some(("real".into(), Some(Duration::ZERO))));
        assert_eq!(parse_answer(br#"{"stream":null}"#), Some((String::new(), None)));
        assert_eq!(parse_answer(br#"{}"#), Some((String::new(), None)));
        assert_eq!(parse_answer(br#"{"stream":5}"#), None);
        assert_eq!(parse_answer(b"{oops"), None);
        assert_eq!(parse_answer(&[0xff, 0xfe]), None);
    }
}
