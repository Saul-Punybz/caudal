//! HTTP routes: recordings API, VOD files, clips.

// Handlers return early with a ready response; boxing it buys nothing.
#![allow(clippy::result_large_err)]

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use caudal_core::media::valid_stream_name;
use caudal_core::{Access, Denied};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::Shared;

use crate::clip::{self, ClipError};
use crate::meta::{self, Meta, parse_segment_name, valid_id};

pub(crate) fn router(shared: Arc<Shared>) -> Router {
    Router::new()
        .route("/api/v1/recordings", get(list))
        .route("/api/v1/recordings/{stream}/{id}", get(get_one).delete(delete_one))
        .route("/vod/{stream}/{id}/{file}", get(vod_file))
        .route("/api/v1/clips", post(clip_route))
        .with_state(shared)
}

fn respond(status: StatusCode, content_type: &'static str, body: impl Into<Body>) -> Response {
    let mut r = (status, body.into()).into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    r.headers_mut().insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    r
}

fn error(status: StatusCode, msg: &str) -> Response {
    respond(status, "text/plain; charset=utf-8", msg.to_owned())
}

fn json<T: serde::Serialize>(v: &T) -> Response {
    match serde_json::to_vec(v) {
        Ok(b) => respond(StatusCode::OK, "application/json", b),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "encoding failed"),
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

/// JWT alphabet only, so a token can never inject text into a playlist.
fn token_is_url_safe(t: &str) -> bool {
    t.len() <= 4096 && t.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

async fn authorize(shared: &Shared, access: Access, stream: &str, token: Option<&str>) -> Result<(), Response> {
    if token.is_some_and(|t| !token_is_url_safe(t)) {
        return Err(error(StatusCode::FORBIDDEN, "bad token"));
    }
    match shared.registry.authorize(access, stream, token).await {
        Ok(()) => Ok(()),
        Err(Denied::Missing) => {
            let mut r = error(StatusCode::UNAUTHORIZED, "token required");
            r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            Err(r)
        }
        Err(Denied::Refused(_)) => Err(error(StatusCode::FORBIDDEN, "forbidden")),
    }
}

fn check_names(stream: &str, id: &str) -> Result<(), Response> {
    if valid_stream_name(stream) && valid_id(id) {
        Ok(())
    } else {
        Err(error(StatusCode::BAD_REQUEST, "invalid stream or recording id"))
    }
}

async fn list(State(shared): State<Arc<Shared>>, RawQuery(q): RawQuery, headers: HeaderMap) -> Response {
    let q = q.unwrap_or_default();
    let token = request_token(&q, &headers);
    let root = shared.cfg.dir.clone();
    let dirs = tokio::task::spawn_blocking(move || crate::recording_dirs(&root)).await.unwrap_or_default();
    let mut out: Vec<Meta> = Vec::new();
    let mut missing = false;
    let mut allowed: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    for (stream, _, dir) in dirs {
        let ok = match allowed.get(&stream) {
            Some(&ok) => ok,
            None => {
                let res = authorize(&shared, Access::Play, &stream, token).await;
                if let Err(r) = &res {
                    missing |= r.status() == StatusCode::UNAUTHORIZED;
                }
                allowed.insert(stream.clone(), res.is_ok());
                res.is_ok()
            }
        };
        if ok && let Some(m) = meta::read_meta(&dir).await {
            out.push(m);
        }
    }
    if out.is_empty() && missing && token.is_none() {
        let mut r = error(StatusCode::UNAUTHORIZED, "token required");
        r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        return r;
    }
    out.sort_by(|a, b| b.started_at.cmp(&a.started_at).then_with(|| b.id.cmp(&a.id)));
    json(&out)
}

async fn get_one(
    State(shared): State<Arc<Shared>>,
    Path((stream, id)): Path<(String, String)>,
    RawQuery(q): RawQuery,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = check_names(&stream, &id) {
        return r;
    }
    let q = q.unwrap_or_default();
    if let Err(r) = authorize(&shared, Access::Play, &stream, request_token(&q, &headers)).await {
        return r;
    }
    match meta::read_meta(&shared.cfg.dir.join(&stream).join(&id)).await {
        Some(m) => json(&m),
        None => error(StatusCode::NOT_FOUND, "no such recording"),
    }
}

async fn delete_one(
    State(shared): State<Arc<Shared>>,
    Path((stream, id)): Path<(String, String)>,
    RawQuery(q): RawQuery,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = check_names(&stream, &id) {
        return r;
    }
    let q = q.unwrap_or_default();
    if let Err(r) = authorize(&shared, Access::Publish, &stream, request_token(&q, &headers)).await {
        return r;
    }
    if shared.active.lock().contains(&(stream.clone(), id.clone())) {
        return error(StatusCode::CONFLICT, "still recording");
    }
    let dir = shared.cfg.dir.join(&stream).join(&id);
    match tokio::fs::remove_dir_all(&dir).await {
        Ok(()) => {
            tracing::info!(%stream, %id, "recording deleted");
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => error(StatusCode::NOT_FOUND, "no such recording"),
        Err(e) => {
            tracing::warn!(%stream, %id, error = %e, "delete failed");
            error(StatusCode::INTERNAL_SERVER_ERROR, "delete failed")
        }
    }
}

async fn vod_file(
    State(shared): State<Arc<Shared>>,
    Path((stream, id, file)): Path<(String, String, String)>,
    RawQuery(q): RawQuery,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = check_names(&stream, &id) {
        return r;
    }
    let content_type = match file.as_str() {
        "index.m3u8" => "application/vnd.apple.mpegurl",
        "init.mp4" => "video/mp4",
        f if parse_segment_name(f).is_some() => "video/iso.segment",
        _ => return error(StatusCode::NOT_FOUND, "no such file"),
    };
    let q = q.unwrap_or_default();
    let token = request_token(&q, &headers);
    if let Err(r) = authorize(&shared, Access::Play, &stream, token).await {
        return r;
    }
    let path = shared.cfg.dir.join(&stream).join(&id).join(&file);
    if file == "index.m3u8" {
        return match tokio::fs::read_to_string(&path).await {
            Ok(text) => {
                let mut r = respond(StatusCode::OK, content_type, with_token(&text, token));
                r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
                r
            }
            Err(_) => error(StatusCode::NOT_FOUND, "no such file"),
        };
    }
    let Ok(f) = tokio::fs::File::open(&path).await else { return error(StatusCode::NOT_FOUND, "no such file") };
    let len = f.metadata().await.map(|m| m.len()).ok();
    let mut r = respond(StatusCode::OK, content_type, Body::from_stream(tokio_util::io::ReaderStream::new(f)));
    if let Some(len) = len {
        r.headers_mut().insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    }
    // Closed segments and init never change.
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("max-age=3600"));
    r
}

/// Every URI in a playlist we serve carries the viewer's token forward.
fn with_token(playlist: &str, token: Option<&str>) -> String {
    let Some(token) = token else { return playlist.to_owned() };
    let add = |uri: &str| format!("{uri}{}token={token}", if uri.contains('?') { '&' } else { '?' });
    let mut out = String::with_capacity(playlist.len() + 64);
    for line in playlist.lines() {
        if line.is_empty() {
        } else if !line.starts_with('#') {
            out.push_str(&add(line));
        } else {
            let mut rest = line;
            while let Some(i) = rest.find("URI=\"") {
                let (head, tail) = rest.split_at(i + 5);
                out.push_str(head);
                let end = tail.find('"').unwrap_or(tail.len());
                out.push_str(&add(&tail[..end]));
                rest = &tail[end..];
            }
            out.push_str(rest);
        }
        out.push('\n');
    }
    out
}

#[derive(Deserialize)]
struct ClipRequest {
    stream: String,
    id: String,
    from_ms: i64,
    to_ms: i64,
}

async fn clip_route(State(shared): State<Arc<Shared>>, RawQuery(q): RawQuery, headers: HeaderMap, body: Bytes) -> Response {
    let Ok(req) = serde_json::from_slice::<ClipRequest>(&body) else {
        return error(StatusCode::BAD_REQUEST, "expected {\"stream\", \"id\", \"from_ms\", \"to_ms\"}");
    };
    if let Err(r) = check_names(&req.stream, &req.id) {
        return r;
    }
    let q = q.unwrap_or_default();
    if let Err(r) = authorize(&shared, Access::Play, &req.stream, request_token(&q, &headers)).await {
        return r;
    }
    let dir = shared.cfg.dir.join(&req.stream).join(&req.id);
    let plan = match clip::plan(&dir, req.from_ms, req.to_ms).await {
        Ok(p) => p,
        Err(ClipError::NotFound) => return error(StatusCode::NOT_FOUND, "no such recording"),
        Err(ClipError::BadRange(why)) => return error(StatusCode::BAD_REQUEST, why),
        Err(ClipError::TooLarge) => return error(StatusCode::PAYLOAD_TOO_LARGE, "clip would exceed 2 GB"),
        Err(ClipError::Io(e)) => {
            tracing::warn!(stream = %req.stream, id = %req.id, error = %e, "clip read failed");
            return error(StatusCode::INTERNAL_SERVER_ERROR, "read failed");
        }
        Err(ClipError::Corrupt(why)) => {
            tracing::warn!(stream = %req.stream, id = %req.id, %why, "clip: unreadable recording");
            return error(StatusCode::INTERNAL_SERVER_ERROR, "recording unreadable");
        }
    };

    // Stream the body: header, then byte ranges copied from the segments.
    let (mut tx, rx) = tokio::io::duplex(256 * 1024);
    let total = plan.total;
    tokio::spawn(async move {
        let res: std::io::Result<()> = async {
            tx.write_all(&plan.header).await?;
            let mut open: Option<(std::path::PathBuf, tokio::fs::File)> = None;
            for p in &plan.pieces {
                if open.as_ref().is_none_or(|(path, _)| *path != p.file) {
                    open = Some((p.file.clone(), tokio::fs::File::open(&p.file).await?));
                }
                let f = &mut open.as_mut().unwrap().1;
                f.seek(std::io::SeekFrom::Start(p.offset)).await?;
                let n = tokio::io::copy(&mut f.take(p.len), &mut tx).await?;
                if n != p.len {
                    return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "segment shorter than planned"));
                }
            }
            tx.shutdown().await
        }
        .await;
        if let Err(e) = res {
            tracing::warn!(error = %e, "clip body aborted");
        }
    });
    let filename = format!("{}-{}-{}-{}.mp4", req.stream, req.id, req.from_ms, req.to_ms);
    let mut r = respond(StatusCode::OK, "video/mp4", Body::from_stream(tokio_util::io::ReaderStream::new(rx)));
    let h = r.headers_mut();
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(total));
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    r
}
