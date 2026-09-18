//! `GET /api/v1/restreams`: one row per configured target.

use axum::Router;
use axum::extract::State;
use axum::routing::get;
use serde::Serialize;

use crate::RestreamHandle;

#[derive(Serialize)]
pub(crate) struct RestreamStatusJson {
    pub stream: String,
    /// `scheme://host/app/****`, never the real key.
    pub target: String,
    pub state: &'static str,
    pub bytes_sent: u64,
    pub since_secs: u64,
    pub last_error: Option<String>,
}

pub(crate) fn router(handle: RestreamHandle) -> Router {
    Router::new().route("/api/v1/restreams", get(list)).with_state(handle)
}

async fn list(State(handle): State<RestreamHandle>) -> axum::Json<Vec<RestreamStatusJson>> {
    axum::Json(handle.targets.iter().map(|t| t.to_json()).collect())
}
