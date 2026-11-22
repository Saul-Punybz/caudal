//! Delivery mechanics for [`crate::Hooks`]: build the Standard Webhooks
//! signed request and retry it with backoff.

use std::time::Duration;

use serde_json::json;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::HookEvent;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Delay before each retry. Three entries: three retries, four attempts
/// total, matching the batch brief ("retry 3 times with backoff 1s/5s/25s").
const BACKOFFS: [Duration; 3] = [Duration::from_secs(1), Duration::from_secs(5), Duration::from_secs(25)];

pub(crate) struct HooksInner {
    pub(crate) urls: Vec<String>,
    pub(crate) webhook: standardwebhooks::Webhook,
    pub(crate) http: reqwest::Client,
}

impl HooksInner {
    pub(crate) fn build(secret: &str) -> Result<standardwebhooks::Webhook, standardwebhooks::WebhookError> {
        standardwebhooks::Webhook::new(secret)
    }

    pub(crate) fn http_client() -> reqwest::Client {
        reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build().unwrap_or_else(|_| reqwest::Client::new())
    }
}

fn event_body(event: &HookEvent) -> serde_json::Value {
    let (event_type, stream) = match event {
        HookEvent::StreamStarted { stream } => ("stream.started", stream),
        HookEvent::StreamEnded { stream } => ("stream.ended", stream),
    };
    // `Rfc3339` formatting only fails for dates outside its representable
    // range, which `now_utc()` never produces; fall back to a fixed epoch
    // string rather than panicking if it somehow ever does.
    let timestamp = OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string());
    json!({
        "type": event_type,
        "timestamp": timestamp,
        "data": { "stream": stream },
    })
}

/// Delivers `event` to every configured URL. Runs on a spawned task; never
/// called on the caller's path.
pub(crate) async fn deliver_all(inner: &HooksInner, event: HookEvent) {
    let body = event_body(&event);
    let payload = match serde_json::to_vec(&body) {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(error = %err, "failed to serialize webhook payload");
            return;
        }
    };

    for url in &inner.urls {
        deliver_one(inner, url, &payload).await;
    }
}

async fn deliver_one(inner: &HooksInner, url: &str, payload: &[u8]) {
    let msg_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
    let timestamp = OffsetDateTime::now_utc().unix_timestamp();
    // The URL itself can be the secret (a Slack incoming webhook carries
    // it in the path): logs show only its scheme and host.
    let shown = caudal_core::net::redact_url_host(url);
    let signature = match inner.webhook.sign(&msg_id, timestamp, payload) {
        Ok(sig) => sig,
        Err(err) => {
            tracing::warn!(url = %shown, error = %err, "failed to sign webhook payload");
            return;
        }
    };

    let mut attempt = 0usize;
    loop {
        attempt += 1;
        let result = inner
            .http
            .post(url)
            .header("content-type", "application/json")
            .header("webhook-id", msg_id.as_str())
            .header("webhook-timestamp", timestamp.to_string())
            .header("webhook-signature", signature.as_str())
            .body(payload.to_vec())
            .send()
            .await;

        match result {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    return;
                }
                if !status.is_server_error() {
                    // 4xx (or an unexpected 1xx/3xx): the receiver rejected
                    // this specific message, retrying won't help.
                    tracing::warn!(url = %shown, %status, "webhook rejected; not retrying");
                    return;
                }
                tracing::warn!(url = %shown, %status, attempt, "webhook delivery failed");
            }
            Err(err) => {
                tracing::warn!(url = %shown, error = %err.without_url(), attempt, "webhook delivery error");
            }
        }

        match BACKOFFS.get(attempt - 1) {
            Some(delay) => tokio::time::sleep(*delay).await,
            None => {
                tracing::warn!(url = %shown, "webhook delivery exhausted retries");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// Every log line written while the guard returned by `capture` lives.
    #[derive(Clone, Default)]
    struct Logs(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Logs {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
        type Writer = Logs;
        fn make_writer(&'a self) -> Logs {
            self.clone()
        }
    }

    #[test]
    fn hooks_config_debug_masks_the_secret_and_url_path() {
        let cfg = crate::HooksConfig {
            urls: vec!["https://hooks.slack.com/services/SLACKSECRET".into()],
            secret: "whsec_HOOKSECRET".into(),
        };
        let shown = format!("{cfg:?}");
        assert!(shown.contains("hooks.slack.com"), "{shown}");
        assert!(!shown.contains("SLACKSECRET") && !shown.contains("HOOKSECRET"), "{shown}");
    }

    #[tokio::test(start_paused = true)]
    async fn delivery_logs_never_show_the_url_path() {
        let logs = Logs::default();
        let subscriber = tracing_subscriber::fmt().with_writer(logs.clone()).with_ansi(false).finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let app = axum::Router::new().fallback(|| async { axum::http::StatusCode::BAD_REQUEST });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        // A port nothing listens on: the error path (and its retries).
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();

        let inner = HooksInner {
            urls: Vec::new(),
            webhook: HooksInner::build("whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw").unwrap(),
            http: HooksInner::http_client(),
        };
        deliver_one(&inner, &format!("http://{addr}/services/T000/B000/SLACKSECRET"), b"{}").await;
        deliver_one(&inner, &format!("http://{closed}/hook/TOKENSECRET?sig=QUERYSECRET"), b"{}").await;

        let out = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        assert!(out.contains("webhook rejected") && out.contains("exhausted retries"), "{out}");
        assert!(out.contains(&format!("http://{addr}/…#")), "{out}");
        for secret in ["SLACKSECRET", "T000", "TOKENSECRET", "QUERYSECRET"] {
            assert!(!out.contains(secret), "{secret} logged:\n{out}");
        }
    }
}
