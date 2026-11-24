//! Blocking authorization webhook: before a publish or a play is allowed,
//! ask an external service, and wait for its answer.
//!
//! This is Caudal's answer to MistServer's blocking triggers (`USER_NEW`,
//! `STREAM_PUSH`), with three differences that come from reading
//! MistServer's code (`docs/research/MISTSERVER-REVERSE.md` §8):
//!
//! - **One short timeout, no retries.** MistServer's downloader waits 5 s per
//!   attempt and tries 5 times, so a dead handler can hold a publish ~25 s.
//!   Here a call gets `timeout` (default 2 s) once; then `on_error` decides.
//! - **The failure answer is explicit** and defaults to deny (fail closed).
//! - **Answers are remembered.** LL-HLS authorizes every playlist and part
//!   request (several per second per viewer), so a decision is cached per
//!   (action, stream, ip, token): allows for `allow_cache`, denials and
//!   failures for `deny_cache`. Concurrent identical requests share one
//!   call instead of each sending their own.
//!
//! Wire format: a POST signed per Standard Webhooks (same as `[hooks]`),
//! body `{"type": "gate.publish" | "gate.play", "timestamp", "data":
//! {"stream", "ip", "token"}}`. The token is passed through so the service
//! can check its own credentials. How the answer is read depends on
//! [`ReplyMode`]:
//!
//! - `status` (default): any 2xx allows.
//! - `mistserver`: a 2xx allows only when its body reads as true under
//!   MistServer's rule ([`mist_bool`]); `false`, `0`, an empty body or
//!   anything else denies. This is how a MistServer blocking trigger
//!   handler answers (`200` with `true`/`false`), so a handler moved over
//!   from MistServer does not let everyone in by answering `200 false`.
//!
//! In both modes 401/403 deny (an optional `{"reason": "..."}` body is
//! logged); anything else, a timeout or a connection error is a failure,
//! decided by `on_error`.

use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::Action;
use crate::blocking::{FlightCache, Signer, deny_reason, small_body};

/// What to do when the service does not answer in time or answers with
/// something other than 2xx/401/403.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnError {
    Deny,
    Allow,
}

/// How a 2xx answer is read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ReplyMode {
    /// The status decides: any 2xx allows, whatever the body.
    #[default]
    Status,
    /// MistServer's blocking-trigger answer: a 2xx allows only when its
    /// body is true per [`mist_bool`].
    MistServer,
}

/// MistServer's `Util::stringToBool` (`lib/bitfields.cpp:62-73` at the
/// commit pinned in `docs/research/MISTSERVER-REVERSE.md`), which
/// `lib/triggers.cpp:291-297` applies to a blocking trigger's answer.
///
/// It drops every whitespace byte (C `isspace`: space, `\t`, `\n`, `\v`,
/// `\f`, `\r`) anywhere in the body, lowercases the first four bytes left,
/// and is true when those start with `1`, `yes`, `true` or `cont`. So
/// `TRUE\n`, ` yes`, `1`, `10`, `continue` and `t r u e` are true; `false`,
/// `0`, `no`, `ok` and an empty body are false.
pub fn mist_bool(body: &[u8]) -> bool {
    let mut head = [0u8; 4];
    let mut n = 0;
    for &b in body {
        if n == head.len() {
            break;
        }
        if !matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') {
            head[n] = b.to_ascii_lowercase();
            n += 1;
        }
    }
    let head = &head[..n];
    [&b"1"[..], b"yes", b"true", b"cont"].iter().any(|p| head.starts_with(p))
}

#[derive(Clone, PartialEq, Eq)]
pub struct GateHookConfig {
    pub url: String,
    /// Standard Webhooks secret (`whsec_...`).
    pub secret: String,
    pub timeout: Duration,
    pub on_error: OnError,
    /// How a 2xx answer is read.
    pub reply: ReplyMode,
    /// Ask on publish.
    pub publish: bool,
    /// Ask on play.
    pub play: bool,
    pub allow_cache: Duration,
    pub deny_cache: Duration,
}

/// Hand-written so `{:?}` never prints `secret` or `url` in full.
impl std::fmt::Debug for GateHookConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GateHookConfig")
            .field("url", &caudal_core::net::redact_url_host(&self.url))
            .field("secret", &"****")
            .field("timeout", &self.timeout)
            .field("on_error", &self.on_error)
            .field("reply", &self.reply)
            .field("publish", &self.publish)
            .field("play", &self.play)
            .field("allow_cache", &self.allow_cache)
            .field("deny_cache", &self.deny_cache)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Decision {
    Allow,
    Deny(String),
    /// The service failed; `on_error` already applied (`true` = allowed).
    Failed(bool),
}

/// Counters for `/metrics`. Survive reloads: the supervisor owns one and
/// hands it to every new [`GateHook`].
#[derive(Debug, Default)]
pub struct GateHookStats {
    /// Calls actually sent to the service, by action and outcome.
    calls: [[AtomicU64; 4]; 2],
    /// Requests answered from the cache (no call sent).
    cached: [AtomicU64; 2],
    /// Total time spent waiting on the service, in microseconds.
    wait_micros: [AtomicU64; 2],
}

/// One row of [`GateHookStats::snapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateHookMetric {
    pub action: &'static str,
    pub outcome: &'static str,
    pub count: u64,
}

const OUTCOMES: [&str; 4] = ["allow", "deny", "error_allowed", "error_denied"];
const ACTIONS: [&str; 2] = ["publish", "play"];

fn action_index(a: Action) -> usize {
    match a {
        Action::Publish => 0,
        Action::Play => 1,
    }
}

impl GateHookStats {
    fn record(&self, action: Action, d: &Decision, waited: Duration) {
        let o = match d {
            Decision::Allow => 0,
            Decision::Deny(_) => 1,
            Decision::Failed(true) => 2,
            Decision::Failed(false) => 3,
        };
        let a = action_index(action);
        self.calls[a][o].fetch_add(1, Ordering::Relaxed);
        self.wait_micros[a].fetch_add(waited.as_micros().min(u64::MAX as u128) as u64, Ordering::Relaxed);
    }

    /// Calls by action and outcome.
    pub fn calls(&self) -> Vec<GateHookMetric> {
        let mut v = Vec::new();
        for (a, action) in ACTIONS.iter().enumerate() {
            for (o, outcome) in OUTCOMES.iter().enumerate() {
                v.push(GateHookMetric { action, outcome, count: self.calls[a][o].load(Ordering::Relaxed) });
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

/// The reply mode is part of the key so a decision read one way is never
/// served to a request that would read it the other way.
type Key = (ReplyMode, Action, String, Option<IpAddr>, Option<String>);

/// The blocking webhook. Cheap to share behind an `Arc`.
pub struct GateHook {
    cfg: GateHookConfig,
    /// `None`: a hook that could not start and refuses everything.
    client: Option<Signer>,
    cache: FlightCache<Key, Decision>,
    stats: Arc<GateHookStats>,
}

impl GateHook {
    /// `Err` on a secret Standard Webhooks cannot parse.
    pub fn new(cfg: GateHookConfig, stats: Arc<GateHookStats>) -> Result<Self, String> {
        let client = Signer::new("[auth.hook]", &cfg.url, &cfg.secret, cfg.timeout)?;
        Ok(Self { cfg, client: Some(client), cache: FlightCache::default(), stats })
    }

    /// A hook that refuses every publish and play: what the server installs
    /// when a configured hook cannot start, so a broken hook fails closed.
    pub fn refusing(stats: Arc<GateHookStats>) -> Self {
        let cfg = GateHookConfig {
            url: String::new(),
            secret: String::new(),
            timeout: Duration::from_millis(100),
            on_error: OnError::Deny,
            reply: ReplyMode::Status,
            publish: true,
            play: true,
            allow_cache: Duration::ZERO,
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

    fn ttl(&self, d: &Decision) -> Duration {
        match d {
            Decision::Allow => self.cfg.allow_cache,
            // Denials and failures (even ones on_error allowed) are asked
            // again sooner, so a recovered service takes over quickly.
            _ => self.cfg.deny_cache,
        }
    }

    /// `Ok` to go ahead; `Err(reason)` to refuse.
    pub async fn check(
        &self,
        action: Action,
        stream: &str,
        token: Option<&str>,
        ip: Option<IpAddr>,
    ) -> Result<(), String> {
        if !self.asks(action) {
            return Ok(());
        }
        if self.client.is_none() {
            return Err("authorization service unavailable".into());
        }
        let key: Key = (self.cfg.reply, action, stream.to_owned(), ip, token.map(str::to_owned));
        let (cell, from_cache) = self.cache.cell(key, |d| self.ttl(d));
        if from_cache {
            self.stats.cached[action_index(action)].fetch_add(1, Ordering::Relaxed);
        }
        let decision = cell.get_or_init(|| self.ask(action, stream, token, ip)).await;
        match decision {
            Decision::Allow | Decision::Failed(true) => Ok(()),
            Decision::Deny(reason) => Err(reason.clone()),
            Decision::Failed(false) => Err("authorization service unavailable".into()),
        }
    }

    async fn ask(&self, action: Action, stream: &str, token: Option<&str>, ip: Option<IpAddr>) -> Decision {
        let started = Instant::now();
        let d = self.call(action, stream, token, ip).await;
        self.stats.record(action, &d, started.elapsed());
        d
    }

    async fn call(&self, action: Action, stream: &str, token: Option<&str>, ip: Option<IpAddr>) -> Decision {
        let failed = |why: &str| {
            let allowed = self.cfg.on_error == OnError::Allow;
            tracing::warn!(
                stream,
                action = action.as_str(),
                why,
                allowed,
                "auth hook failed; on_error = {}",
                if allowed { "allow" } else { "deny" },
            );
            Decision::Failed(allowed)
        };
        let Some(client) = &self.client else { return failed("not started") };
        let data = json!({ "stream": stream, "ip": ip.map(|i| i.to_string()), "token": token });
        let resp = match client.post(&format!("gate.{}", action.as_str()), data).await {
            Ok(r) => r,
            Err(e) => return failed(e.as_str()),
        };
        let status = resp.status();
        if status.is_success() {
            if self.cfg.reply == ReplyMode::Status {
                return Decision::Allow;
            }
            let Some(body) = small_body(resp, self.cfg.timeout).await else {
                return failed("unreadable answer body");
            };
            if mist_bool(&body) {
                return Decision::Allow;
            }
            tracing::info!(stream, action = action.as_str(), %status, "auth hook denied (MistServer-style false)");
            return Decision::Deny("refused by the authorization service".into());
        }
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            let reason = deny_reason(resp, self.cfg.timeout)
                .await
                .unwrap_or_else(|| "refused by the authorization service".into());
            tracing::info!(stream, action = action.as_str(), %status, reason, "auth hook denied");
            return Decision::Deny(reason);
        }
        failed(status.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::mist_bool;

    #[test]
    fn mist_bool_matches_mistserver_string_to_bool() {
        for t in ["true", "TRUE\n", " yes", "Yes please", "1", "10", "cont", "continue", "t r u e", "\r\n\x0btrue"] {
            assert!(mist_bool(t.as_bytes()), "{t:?} is true");
        }
        for f in ["", " \n", "false", "0", "no", "ok", "y", "tru", "yeah", "allow", "\0true", "on", "-1"] {
            assert!(!mist_bool(f.as_bytes()), "{f:?} is false");
        }
        // Only the first four non-whitespace bytes are looked at.
        assert!(mist_bool(b"truefalse"));
        assert!(!mist_bool(b"\xc3\xa9true"));
    }
}
