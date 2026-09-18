//! `GET /api/v1/alerts`: currently active alerts, plus the last 100
//! alert/resolved events, newest last.

use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::Shared;

#[derive(Debug, Serialize)]
pub struct ActiveAlertJson {
    pub rule: &'static str,
    pub stream: String,
    pub value: f64,
    pub threshold: f64,
    /// When this alert fired, RFC 3339.
    pub since: String,
}

#[derive(Debug, Serialize)]
pub struct AlertEventJson {
    /// `"alert"` or `"resolved"`.
    pub event: &'static str,
    pub rule: &'static str,
    pub stream: String,
    pub value: f64,
    pub threshold: f64,
    pub at: String,
}

#[derive(Debug, Serialize)]
struct AlertsResponse {
    active: Vec<ActiveAlertJson>,
    events: Vec<AlertEventJson>,
}

async fn alerts(State(shared): State<Arc<Shared>>) -> Json<AlertsResponse> {
    let active = shared
        .active
        .lock()
        .iter()
        .map(|((stream, rule), info)| ActiveAlertJson {
            rule,
            stream: stream.clone(),
            value: info.value,
            threshold: info.threshold,
            since: info.since.clone(),
        })
        .collect();
    let events = shared
        .events
        .lock()
        .iter()
        .map(|e| AlertEventJson {
            event: e.event,
            rule: e.rule,
            stream: e.stream.clone(),
            value: e.value,
            threshold: e.threshold,
            at: e.at.clone(),
        })
        .collect();
    Json(AlertsResponse { active, events })
}

pub(crate) fn router(shared: Arc<Shared>) -> Router {
    Router::new().route("/api/v1/alerts", get(alerts)).with_state(shared)
}
