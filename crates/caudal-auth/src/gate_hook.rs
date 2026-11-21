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
//! can check its own credentials. Answer: any 2xx allows; 401/403 deny
//! (an optional `{"reason": "..."}` body is logged); anything else, a
//! timeout or a connection error is a failure, decided by `on_error`.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::json;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::OnceCell;

use crate::Action;

/// Cached decisions kept before expired ones are swept.
const SWEEP_AT: usize = 10_000;
/// Longest denial reason kept from the service's answer (for logs).
const MAX_REASON: usize = 200;

/// What to do when the service does not answer in time or answers with
/// something other than 2xx/401/403.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnError {
    Deny,
    Allow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateHookConfig {
    pub url: String,
    /// Standard Webhooks secret (`whsec_...`).
    pub secret: String,
    pub timeout: Duration,
    pub on_error: OnError,
    /// Ask on publish.
    pub publish: bool,
    /// Ask on play.
    pub play: bool,
    pub allow_cache: Duration,
    pub deny_cache: Duration,
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

type Key = (Action, String, Option<IpAddr>, Option<String>);

struct Entry {
    created: Instant,
    cell: Arc<OnceCell<Decision>>,
}

/// The blocking webhook. Cheap to share behind an `Arc`.
pub struct GateHook {
    cfg: GateHookConfig,
    /// `None`: a hook that could not start and refuses everything.
    client: Option<(standardwebhooks::Webhook, reqwest::Client)>,
    cache: Mutex<HashMap<Key, Entry>>,
    stats: Arc<GateHookStats>,
}

impl GateHook {
    /// `Err` on a secret Standard Webhooks cannot parse.
    pub fn new(cfg: GateHookConfig, stats: Arc<GateHookStats>) -> Result<Self, String> {
        let webhook = standardwebhooks::Webhook::new(&cfg.secret).map_err(|e| format!("[auth.hook] secret: {e}"))?;
        let http = reqwest::Client::builder()
            .timeout(cfg.timeout)
            .connect_timeout(cfg.timeout)
            // A redirect would send the token somewhere the config never named.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| format!("[auth.hook] http client: {e}"))?;
        Ok(Self { cfg, client: Some((webhook, http)), cache: Mutex::new(HashMap::new()), stats })
    }

    /// A hook that refuses every publish and play: what the server installs
    /// when a configured hook cannot start, so a broken hook fails closed.
    pub fn refusing(stats: Arc<GateHookStats>) -> Self {
        let cfg = GateHookConfig {
            url: String::new(),
            secret: String::new(),
            timeout: Duration::from_millis(100),
            on_error: OnError::Deny,
            publish: true,
            play: true,
            allow_cache: Duration::ZERO,
            deny_cache: Duration::ZERO,
        };
        Self { cfg, client: None, cache: Mutex::new(HashMap::new()), stats }
    }

    fn asks(&self, action: Action) -> bool {
        match action {
            Action::Publish => self.cfg.publish,
            Action::Play => self.cfg.play,
        }
    }

    fn fresh(&self, e: &Entry, now: Instant) -> bool {
        let ttl = match e.cell.get() {
            None => return true, // in flight: share it
            Some(Decision::Allow) => self.cfg.allow_cache,
            // Denials and failures (even ones on_error allowed) are asked
            // again sooner, so a recovered service takes over quickly.
            Some(_) => self.cfg.deny_cache,
        };
        now.duration_since(e.created) < ttl
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
        let key: Key = (action, stream.to_owned(), ip, token.map(str::to_owned));
        let now = Instant::now();
        let (cell, from_cache) = {
            let mut cache = self.cache.lock();
            if cache.len() >= SWEEP_AT {
                cache.retain(|_, e| self.fresh(e, now));
                if cache.len() >= SWEEP_AT {
                    // Still full of live entries: start over rather than grow.
                    cache.clear();
                }
            }
            match cache.get(&key) {
                Some(e) if self.fresh(e, now) => (e.cell.clone(), e.cell.get().is_some()),
                _ => {
                    let cell = Arc::new(OnceCell::new());
                    cache.insert(key, Entry { created: now, cell: cell.clone() });
                    (cell, false)
                }
            }
        };
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
        let timestamp = OffsetDateTime::now_utc();
        let body = json!({
            "type": format!("gate.{}", action.as_str()),
            "timestamp": timestamp.format(&Rfc3339).unwrap_or_default(),
            "data": { "stream": stream, "ip": ip.map(|i| i.to_string()), "token": token },
        });
        let Some((webhook, http)) = &self.client else { return failed("not started") };
        let Ok(payload) = serde_json::to_vec(&body) else { return failed("serialize") };
        let msg_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        let Ok(signature) = webhook.sign(&msg_id, timestamp.unix_timestamp(), &payload) else {
            return failed("sign");
        };
        let sent = http
            .post(&self.cfg.url)
            .header("content-type", "application/json")
            .header("webhook-id", msg_id.as_str())
            .header("webhook-timestamp", timestamp.unix_timestamp().to_string())
            .header("webhook-signature", signature.as_str())
            .body(payload)
            .send()
            .await;
        let resp = match sent {
            Ok(r) => r,
            Err(e) if e.is_timeout() => return failed("timeout"),
            Err(_) => return failed("connection"),
        };
        let status = resp.status();
        if status.is_success() {
            return Decision::Allow;
        }
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            let text = tokio::time::timeout(self.cfg.timeout, resp.text()).await.ok().and_then(Result::ok);
            let reason = text
                .and_then(|t| {
                    serde_json::from_str::<serde_json::Value>(&t)
                        .ok()
                        .and_then(|v| v.get("reason").and_then(|r| r.as_str()).map(str::to_owned))
                })
                .map(|r| r.chars().take(MAX_REASON).collect::<String>())
                .unwrap_or_else(|| "refused by the authorization service".into());
            tracing::info!(stream, action = action.as_str(), %status, reason, "auth hook denied");
            return Decision::Deny(reason);
        }
        failed(status.as_str())
    }
}
