//! `GET /api/v1/failover` and `POST /api/v1/failover/{stream}/switch`.

use axum::Router;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Path, RawQuery, State};
use axum::http::Extensions;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use caudal_core::{Access, Denied};
use serde::Deserialize;

use crate::{FailoverHandle, SwitchError};

pub(crate) fn router(handle: FailoverHandle) -> Router {
    Router::new()
        .route("/api/v1/failover", get(list))
        .route("/api/v1/failover/{stream}/switch", post(switch))
        .with_state(handle)
}

async fn list(State(handle): State<FailoverHandle>) -> Response {
    match serde_json::to_vec(&handle.status()) {
        Ok(body) => {
            let mut r = (StatusCode::OK, body).into_response();
            r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
            r
        }
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "encoding failed").into_response(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SwitchBody {
    /// A configured source (`name` or `file:<path>`), or null for automatic.
    source: Option<String>,
}

async fn switch(
    State(handle): State<FailoverHandle>,
    Path(stream): Path<String>,
    extensions: Extensions,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !handle.has(&stream) {
        return (StatusCode::NOT_FOUND, "no such failover stream").into_response();
    }
    let token = request_token(query.as_deref().unwrap_or(""), &headers);
    let ip = extensions.get::<ConnectInfo<std::net::SocketAddr>>().map(|ConnectInfo(p)| {
        let xff = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok());
        caudal_core::net::resolve_forwarded(p.ip(), xff, handle.trusted_proxies())
    });
    match handle.registry().authorize(Access::Publish, &stream, token, ip).await {
        Ok(()) => {}
        Err(Denied::Missing) => {
            let mut r = (StatusCode::UNAUTHORIZED, "token required").into_response();
            r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            return r;
        }
        Err(Denied::Refused(_)) => return (StatusCode::FORBIDDEN, "forbidden").into_response(),
    }
    let Ok(req) = serde_json::from_slice::<SwitchBody>(&body) else {
        return (StatusCode::BAD_REQUEST, "expected {\"source\": \"<name>\" | null}").into_response();
    };
    match handle.switch(&stream, req.source).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(SwitchError::UnknownStream) => (StatusCode::NOT_FOUND, "no such failover stream").into_response(),
        Err(SwitchError::UnknownSource) => {
            (StatusCode::BAD_REQUEST, "not a source of this failover stream").into_response()
        }
        Err(SwitchError::NotHealthy) => (StatusCode::CONFLICT, "source is not healthy").into_response(),
    }
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
