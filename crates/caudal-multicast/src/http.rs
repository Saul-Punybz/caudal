//! `GET /api/v1/multicast`: one row per configured output.

use axum::Router;
use axum::extract::State;
use axum::routing::get;

use crate::MulticastHandle;
use crate::status::OutputJson;

pub(crate) fn router(handle: MulticastHandle) -> Router {
    Router::new().route("/api/v1/multicast", get(list)).with_state(handle)
}

async fn list(State(handle): State<MulticastHandle>) -> axum::Json<Vec<OutputJson>> {
    axum::Json(handle.statuses())
}
