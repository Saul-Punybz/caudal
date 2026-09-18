//! A small JWKS cache: fetch, keep by `kid`, refresh on a timer, and
//! rate-limit the extra fetch triggered by an unknown `kid` so a hostile or
//! confused client can't turn key lookups into a fetch storm.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use jsonwebtoken::jwk::{Jwk, JwkSet};
use tokio::sync::RwLock;

/// How often an unknown `kid` is allowed to trigger an out-of-band refetch.
const UNKNOWN_KID_RATE_LIMIT: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct JwksCache {
    url: String,
    http: reqwest::Client,
    keys: RwLock<HashMap<String, Jwk>>,
    last_unknown_kid_fetch: Mutex<Option<Instant>>,
}

impl JwksCache {
    /// Builds the cache and, if a tokio runtime is available, spawns the
    /// background task that fetches now and then every `refresh`. Without a
    /// runtime the cache starts empty and only ever grows through the
    /// rate-limited unknown-`kid` path in [`Self::get`].
    pub(crate) fn spawn(url: String, refresh: Duration) -> std::sync::Arc<Self> {
        let http =
            reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build().unwrap_or_else(|_| reqwest::Client::new());
        let cache = std::sync::Arc::new(Self {
            url,
            http,
            keys: RwLock::new(HashMap::new()),
            last_unknown_kid_fetch: Mutex::new(None),
        });

        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let cache = cache.clone();
                handle.spawn(async move {
                    loop {
                        if let Err(err) = cache.refresh_now().await {
                            tracing::warn!(url = %cache.url, error = %err, "jwks fetch failed, keeping last good keys");
                        }
                        tokio::time::sleep(refresh).await;
                    }
                });
            }
            Err(_) => {
                tracing::warn!("no tokio runtime available; jwks background refresh disabled");
            }
        }

        cache
    }

    /// Looks up `kid`. On a miss, tries one rate-limited refetch before
    /// giving up.
    pub(crate) async fn get(&self, kid: &str) -> Option<Jwk> {
        if let Some(jwk) = self.keys.read().await.get(kid).cloned() {
            return Some(jwk);
        }
        self.maybe_refetch_unknown_kid().await;
        self.keys.read().await.get(kid).cloned()
    }

    async fn maybe_refetch_unknown_kid(&self) {
        let allowed = {
            let mut last = self.last_unknown_kid_fetch.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            let allowed = !matches!(*last, Some(t) if now.duration_since(t) < UNKNOWN_KID_RATE_LIMIT);
            if allowed {
                *last = Some(now);
            }
            allowed
        };
        if !allowed {
            return;
        }
        if let Err(err) = self.refresh_now().await {
            tracing::warn!(url = %self.url, error = %err, "jwks refetch on unknown kid failed");
        }
    }

    async fn refresh_now(&self) -> Result<(), reqwest::Error> {
        let fetched = self.fetch().await?;
        *self.keys.write().await = fetched;
        Ok(())
    }

    async fn fetch(&self) -> Result<HashMap<String, Jwk>, reqwest::Error> {
        let set: JwkSet = self.http.get(&self.url).send().await?.error_for_status()?.json().await?;
        Ok(set.keys.into_iter().filter_map(|k| k.common.key_id.clone().map(|kid| (kid, k))).collect())
    }
}
