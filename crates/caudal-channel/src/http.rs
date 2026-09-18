//! `GET /api/v1/channels` and `POST /api/v1/channels/{name}/skip`.

use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use caudal_core::{Access, Denied};

use crate::ChannelHandle;

pub(crate) fn router(handle: ChannelHandle) -> Router {
    Router::new()
        .route("/api/v1/channels", get(list))
        .route("/api/v1/channels/{name}/skip", post(skip))
        .with_state(handle)
}

async fn list(State(handle): State<ChannelHandle>) -> Response {
    match serde_json::to_vec(&handle.status()) {
        Ok(body) => {
            let mut r = (StatusCode::OK, body).into_response();
            r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
            r
        }
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "encoding failed").into_response(),
    }
}

async fn skip(
    State(handle): State<ChannelHandle>,
    Path(name): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    if handle.find(&name).is_none() {
        return (StatusCode::NOT_FOUND, "no such channel").into_response();
    }
    let token = request_token(query.as_deref().unwrap_or(""), &headers);
    match handle.registry().authorize(Access::Publish, &name, token).await {
        Ok(()) => {}
        Err(Denied::Missing) => {
            let mut r = (StatusCode::UNAUTHORIZED, "token required").into_response();
            r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            return r;
        }
        Err(Denied::Refused(_)) => return (StatusCode::FORBIDDEN, "forbidden").into_response(),
    }
    handle.skip(&name);
    StatusCode::NO_CONTENT.into_response()
}

/// `?token=` wins over `Authorization: Bearer`.
fn request_token<'a>(query: &'a str, headers: &'a HeaderMap) -> Option<&'a str> {
    let from_query = query.split('&').find_map(|kv| kv.strip_prefix("token=")).filter(|t| !t.is_empty());
    from_query.or_else(|| {
        headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim)
            .filter(|t| !t.is_empty())
    })
}
