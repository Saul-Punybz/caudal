//! What the blocking webhooks (`[auth.hook]`, `[auth.rewrite]`,
//! `[auth.source]`) share: a
//! signed POST with one short timeout, and a cache of answers in which
//! concurrent identical requests share one call.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::OnceCell;

/// Cached answers kept before expired ones are swept.
const SWEEP_AT: usize = 10_000;
/// Longest denial reason kept from the service's answer (for logs).
const MAX_REASON: usize = 200;
/// Largest answer body read from a service.
pub(crate) const MAX_BODY: usize = 4096;

/// Why a call got no usable HTTP response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PostError {
    Timeout,
    Connection,
    Serialize,
    Sign,
}

impl PostError {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            PostError::Timeout => "timeout",
            PostError::Connection => "connection",
            PostError::Serialize => "serialize",
            PostError::Sign => "sign",
        }
    }
}

/// Signs and sends one Standard Webhooks POST: one attempt, `timeout` for
/// connecting and for the whole exchange, redirects never followed (a
/// redirect would send the client's token somewhere the config never
/// named).
pub(crate) struct Signer {
    webhook: standardwebhooks::Webhook,
    http: reqwest::Client,
    url: String,
}

impl Signer {
    /// `section` names the config section in errors (`[auth.hook]`).
    pub(crate) fn new(section: &str, url: &str, secret: &str, timeout: Duration) -> Result<Self, String> {
        let webhook = standardwebhooks::Webhook::new(secret).map_err(|e| format!("{section} secret: {e}"))?;
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| format!("{section} http client: {e}"))?;
        Ok(Self { webhook, http, url: url.to_owned() })
    }

    /// POSTs `{"type": kind, "timestamp": ..., "data": data}`.
    pub(crate) async fn post(&self, kind: &str, data: serde_json::Value) -> Result<reqwest::Response, PostError> {
        let timestamp = OffsetDateTime::now_utc();
        let body = serde_json::json!({
            "type": kind,
            "timestamp": timestamp.format(&Rfc3339).unwrap_or_default(),
            "data": data,
        });
        let payload = serde_json::to_vec(&body).map_err(|_| PostError::Serialize)?;
        let msg_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        let signature =
            self.webhook.sign(&msg_id, timestamp.unix_timestamp(), &payload).map_err(|_| PostError::Sign)?;
        self.http
            .post(&self.url)
            .header("content-type", "application/json")
            .header("webhook-id", msg_id.as_str())
            .header("webhook-timestamp", timestamp.unix_timestamp().to_string())
            .header("webhook-signature", signature.as_str())
            .body(payload)
            .send()
            .await
            .map_err(|e| if e.is_timeout() { PostError::Timeout } else { PostError::Connection })
    }
}

struct Entry<V> {
    created: Instant,
    cell: Arc<OnceCell<V>>,
}

/// Answers by key, each kept for as long as `ttl(answer)` says. A request
/// that finds a call in flight for its key waits for that call instead of
/// sending its own.
pub(crate) struct FlightCache<K, V> {
    map: Mutex<HashMap<K, Entry<V>>>,
}

impl<K: Hash + Eq, V> Default for FlightCache<K, V> {
    fn default() -> Self {
        Self { map: Mutex::new(HashMap::new()) }
    }
}

impl<K: Hash + Eq, V> FlightCache<K, V> {
    fn fresh(e: &Entry<V>, now: Instant, ttl: &impl Fn(&V) -> Duration) -> bool {
        match e.cell.get() {
            None => true, // in flight: share it
            Some(v) => now.duration_since(e.created) < ttl(v),
        }
    }

    /// The cell holding `key`'s answer, and whether it already holds one
    /// (answered from the cache). An empty cell is to be filled with
    /// `OnceCell::get_or_init`, which runs the call once for every waiter.
    pub(crate) fn cell(&self, key: K, ttl: impl Fn(&V) -> Duration) -> (Arc<OnceCell<V>>, bool) {
        let now = Instant::now();
        let mut map = self.map.lock();
        if map.len() >= SWEEP_AT {
            map.retain(|_, e| Self::fresh(e, now, &ttl));
            if map.len() >= SWEEP_AT {
                // Still full of live entries: start over rather than grow.
                map.clear();
            }
        }
        match map.get(&key) {
            Some(e) if Self::fresh(e, now, &ttl) => (e.cell.clone(), e.cell.get().is_some()),
            _ => {
                let cell = Arc::new(OnceCell::new());
                map.insert(key, Entry { created: now, cell: cell.clone() });
                (cell, false)
            }
        }
    }
}

/// The body of `resp`, read within `timeout`; `None` past [`MAX_BODY`] bytes,
/// on a timeout or on a read error.
pub(crate) async fn small_body(resp: reqwest::Response, timeout: Duration) -> Option<Vec<u8>> {
    if resp.content_length().is_some_and(|n| n > MAX_BODY as u64) {
        return None;
    }
    let read = async {
        let mut resp = resp;
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await.ok()? {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_BODY {
                return None;
            }
        }
        Some(body)
    };
    tokio::time::timeout(timeout, read).await.ok().flatten()
}

/// The `reason` of a `{"reason": "..."}` denial body, cut to a log-friendly
/// length.
pub(crate) async fn deny_reason(resp: reqwest::Response, timeout: Duration) -> Option<String> {
    let body = small_body(resp, timeout).await?;
    let v = serde_json::from_slice::<serde_json::Value>(&body).ok()?;
    Some(v.get("reason")?.as_str()?.chars().take(MAX_REASON).collect())
}
