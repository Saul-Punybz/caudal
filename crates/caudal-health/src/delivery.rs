//! Signed delivery of alert/resolved events, on the same wire format as
//! `caudal-auth`'s hooks: a Standard Webhooks signature in
//! `webhook-id`/`webhook-timestamp`/`webhook-signature`, JSON body, retried
//! with backoff on 5xx, given up on 4xx.
//!
//! Fed through a bounded channel so a slow or wedged receiver can never
//! block the tick loop that watches media: [`Delivery::send`] is
//! non-blocking and drops the event (with a warning) if the queue is full.

use std::time::Duration;

use serde::Serialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::mpsc;

/// Payload shape: `{event, rule, stream, value, threshold, at}`, plus a
/// `text` field so a Slack incoming-webhook URL renders something readable
/// (Slack looks for a top-level `text` key; everything else is ignored by
/// it and read normally by any other receiver).
#[derive(Debug, Clone, Serialize)]
pub struct AlertPayload {
    pub event: &'static str, // "alert" | "resolved"
    pub rule: &'static str,
    pub stream: String,
    pub value: f64,
    pub threshold: f64,
    pub at: String,
    pub text: String,
}

impl AlertPayload {
    pub fn new(event: &'static str, rule: &'static str, stream: &str, value: f64, threshold: f64) -> Self {
        let at = OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string());
        let text = format!(
            "[caudal] {} {rule} on {stream}: value {value:.2}, threshold {threshold:.2}",
            if event == "alert" { "ALERT" } else { "RESOLVED" }
        );
        Self { event, rule, stream: stream.to_owned(), value, threshold, at, text }
    }
}

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Three retries (four attempts total), same cadence as caudal-auth's hooks.
const BACKOFFS: [Duration; 3] = [Duration::from_secs(1), Duration::from_secs(5), Duration::from_secs(25)];
/// Events queued for delivery before new ones are dropped. Alerts are rare
/// state transitions, not a per-frame signal, so this is generous headroom
/// rather than a tight budget.
const QUEUE_CAPACITY: usize = 256;

pub struct Delivery {
    tx: mpsc::Sender<AlertPayload>,
}

impl Delivery {
    /// Starts the delivery worker. Must be called inside a tokio runtime.
    pub fn start(urls: Vec<String>, secret: &str) -> Result<Self, String> {
        let webhook = standardwebhooks::Webhook::new(secret).map_err(|e| e.to_string())?;
        let http = reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build().map_err(|e| e.to_string())?;
        let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
        tokio::spawn(worker(rx, urls, webhook, http));
        Ok(Self { tx })
    }

    /// Enqueues `payload` for delivery to every configured URL. Never
    /// blocks: a full queue drops the event and logs a warning.
    pub fn send(&self, payload: AlertPayload) {
        if self.tx.try_send(payload).is_err() {
            tracing::warn!("alert delivery queue full or closed; dropping event");
        }
    }
}

async fn worker(
    mut rx: mpsc::Receiver<AlertPayload>,
    urls: Vec<String>,
    webhook: standardwebhooks::Webhook,
    http: reqwest::Client,
) {
    while let Some(payload) = rx.recv().await {
        let body = match serde_json::to_vec(&payload) {
            Ok(b) => b,
            Err(err) => {
                tracing::warn!(error = %err, "failed to serialize alert payload");
                continue;
            }
        };
        for url in &urls {
            deliver_one(&http, &webhook, url, &body).await;
        }
    }
}

async fn deliver_one(http: &reqwest::Client, webhook: &standardwebhooks::Webhook, url: &str, payload: &[u8]) {
    let msg_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
    let timestamp = OffsetDateTime::now_utc().unix_timestamp();
    let signature = match webhook.sign(&msg_id, timestamp, payload) {
        Ok(sig) => sig,
        Err(err) => {
            tracing::warn!(url, error = %err, "failed to sign alert payload");
            return;
        }
    };

    let mut attempt = 0usize;
    loop {
        attempt += 1;
        let result = http
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
                    tracing::warn!(url, %status, "alert webhook rejected; not retrying");
                    return;
                }
                tracing::warn!(url, %status, attempt, "alert webhook delivery failed");
            }
            Err(err) => {
                tracing::warn!(url, error = %err, attempt, "alert webhook delivery error");
            }
        }

        match BACKOFFS.get(attempt - 1) {
            Some(delay) => tokio::time::sleep(*delay).await,
            None => {
                tracing::warn!(url, "alert webhook delivery exhausted retries");
                return;
            }
        }
    }
}
