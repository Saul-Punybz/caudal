//! The gate middleware and the `/api/v1/auth/*` routes.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Instant;

use axum::Router;
use axum::extract::{ConnectInfo, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::json;

use crate::password::{match_token, verify_dummy, verify_password};
use crate::{Admin, oidc};

pub const SESSION_COOKIE: &str = "caudal_session";
const OIDC_COOKIE: &str = "caudal_oidc";
const OIDC_COOKIE_PATH: &str = "/api/v1/auth/oidc";
/// Header carrying the session's CSRF token on state-changing requests.
pub const CSRF_HEADER: &str = "x-csrf-token";
/// Set on the gate's 401s, so the UI can tell "log in again" apart from a
/// stream token refused by a route's own publish/play check.
pub const LOGIN_HEADER: &str = "x-caudal-login";

/// Marks requests that arrived on Caudal's own HTTPS listener, so the
/// session cookie gets `Secure`. Insert with `Extension(ViaTls)` on the
/// TLS copy of the app.
#[derive(Debug, Clone, Copy)]
pub struct ViaTls;

/// The admin who made a request, inserted by the gate for handlers that
/// want it.
#[derive(Debug, Clone)]
pub struct AdminUser(pub String);

/// Auth routes a signed-out browser must reach.
const PUBLIC_AUTH: [&str; 4] =
    ["/api/v1/auth/session", "/api/v1/auth/login", "/api/v1/auth/oidc/start", crate::OIDC_CALLBACK_PATH];

/// Media routes that stay public: each checks its own publish/play token
/// (`[auth]`) where one is required. Players and encoders cannot log in.
const PUBLIC_PREFIXES: [&str; 6] = ["/hls/", "/play/", "/vod/", "/whip/", "/whep/", "/moq/"];

/// Whether a request needs an admin. Everything under `/api/` does
/// (except logging in), `/metrics` does unless `public_metrics`; media and
/// probes do not; the UI's static files are public for `GET`/`HEAD` (they
/// are the open-source bundle and hold no data) and any other method on a
/// path nobody claimed is refused, so a future route defaults to closed.
pub fn needs_admin(method: &Method, path: &str, public_metrics: bool) -> bool {
    if PUBLIC_AUTH.contains(&path) {
        return false;
    }
    if path == "/api" || path.starts_with("/api/") {
        return true;
    }
    if path == "/metrics" {
        return !public_metrics;
    }
    if path == "/healthz" || path == "/readyz" || PUBLIC_PREFIXES.iter().any(|p| path.starts_with(p)) {
        return false;
    }
    !(method == Method::GET || method == Method::HEAD)
}

/// Wraps the whole app (merge every router first, then protect).
pub fn protect(app: Router, admin: Admin) -> Router {
    app.layer(axum::middleware::from_fn_with_state(admin, guard))
}

/// `/api/v1/auth/*`. Mount it with or without login configured: without,
/// `/api/v1/auth/session` tells the UI no login is required.
pub fn router(admin: Option<Admin>) -> Router {
    Router::new()
        .route("/api/v1/auth/session", get(session))
        .route("/api/v1/auth/login", post(login))
        .route("/api/v1/auth/logout", post(logout))
        .route("/api/v1/auth/oidc/start", get(oidc_start))
        .route(crate::OIDC_CALLBACK_PATH, get(oidc_callback))
        .with_state(admin)
}

fn is_safe(m: &Method) -> bool {
    matches!(*m, Method::GET | Method::HEAD | Method::OPTIONS)
}

fn json_error(status: StatusCode, msg: &str) -> Response {
    (status, axum::Json(json!({ "error": msg }))).into_response()
}

fn login_required() -> Response {
    let mut r = json_error(StatusCode::UNAUTHORIZED, "login required");
    r.headers_mut().insert(LOGIN_HEADER, HeaderValue::from_static("required"));
    r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer realm=\"caudal\""));
    r
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ").map(str::trim).filter(|t| !t.is_empty())
}

fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
        .filter(|v| !v.is_empty())
}

fn client_ip(req: &Request) -> IpAddr {
    req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip()).unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
}

fn secure(admin: &Admin, req: &Request) -> bool {
    admin.0.secure_cookies || req.extensions().get::<ViaTls>().is_some()
}

fn set_cookie(name: &str, value: &str, path: &str, max_age: u64, secure: bool) -> HeaderValue {
    let s = if secure { "; Secure" } else { "" };
    HeaderValue::from_str(&format!("{name}={value}; Path={path}; HttpOnly; SameSite=Lax; Max-Age={max_age}{s}"))
        .expect("cookie values are base64url")
}

async fn guard(State(admin): State<Admin>, mut req: Request, next: Next) -> Response {
    if !needs_admin(req.method(), req.uri().path(), admin.0.public_metrics) {
        return next.run(req).await;
    }
    // A bearer that is not an admin token may still be a stream token for
    // the route's own check (channel skip), so fall through to the cookie.
    if let Some(name) = bearer(req.headers()).and_then(|t| match_token(t, &admin.0.tokens)) {
        let user = AdminUser(format!("token:{name}"));
        req.extensions_mut().insert(user);
        return next.run(req).await;
    }
    let session = cookie(req.headers(), SESSION_COOKIE).and_then(|id| admin.0.sessions.get(id, Instant::now()));
    let Some(session) = session else { return login_required() };
    if !is_safe(req.method()) {
        let presented = req.headers().get(CSRF_HEADER).and_then(|v| v.to_str().ok());
        if !session.csrf_ok(presented) {
            return json_error(StatusCode::FORBIDDEN, "missing or wrong X-CSRF-Token");
        }
    }
    req.extensions_mut().insert(AdminUser(session.user));
    next.run(req).await
}

async fn session(State(admin): State<Option<Admin>>, headers: HeaderMap) -> Response {
    let Some(admin) = admin else {
        return axum::Json(json!({
            "required": false, "authenticated": false, "user": null, "csrf_token": null,
            "password": false, "oidc": false,
        }))
        .into_response();
    };
    let s = cookie(&headers, SESSION_COOKIE).and_then(|id| admin.0.sessions.get(id, Instant::now()));
    axum::Json(json!({
        "required": true,
        "authenticated": s.is_some(),
        "user": s.as_ref().map(|s| s.user.clone()),
        "csrf_token": s.as_ref().map(|s| s.csrf.clone()),
        "password": !admin.0.users.is_empty(),
        "oidc": admin.0.oidc.is_some(),
    }))
    .into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginBody {
    name: String,
    password: String,
}

fn rate_limited(retry: std::time::Duration) -> Response {
    let mut r = json_error(StatusCode::TOO_MANY_REQUESTS, "too many attempts, try again later");
    r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(retry.as_secs().max(1)));
    r
}

async fn login(State(admin): State<Option<Admin>>, req: Request) -> Response {
    let Some(admin) = admin else { return StatusCode::NOT_FOUND.into_response() };
    // JSON only: a cross-site HTML form cannot send it without a CORS
    // preflight, which Caudal never grants (login CSRF).
    let is_json = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(';').next().is_some_and(|m| m.trim().eq_ignore_ascii_case("application/json")));
    if !is_json {
        return json_error(StatusCode::UNSUPPORTED_MEDIA_TYPE, "send application/json");
    }
    let ip = client_ip(&req);
    if let Err(retry) = admin.0.limiter.hit(ip, Instant::now()) {
        tracing::warn!(%ip, "admin login rate limited");
        return rate_limited(retry);
    }
    let secure = secure(&admin, &req);
    let Ok(bytes) = axum::body::to_bytes(req.into_body(), 16 * 1024).await else {
        return json_error(StatusCode::BAD_REQUEST, "body too large");
    };
    let Ok(body) = serde_json::from_slice::<LoginBody>(&bytes) else {
        return json_error(StatusCode::BAD_REQUEST, "expected {\"name\", \"password\"}");
    };
    let phc = admin.0.users.get(&body.name).cloned();
    let name = body.name.clone();
    // argon2 is deliberately slow; keep it off the async workers.
    let ok = tokio::task::spawn_blocking(move || match phc {
        Some(phc) => verify_password(&phc, &body.password),
        None => {
            verify_dummy(&body.password);
            false
        }
    })
    .await
    .unwrap_or(false);
    if !ok {
        tracing::warn!(%ip, user = %name, "admin login failed");
        return json_error(StatusCode::UNAUTHORIZED, "wrong name or password");
    }
    tracing::info!(%ip, user = %name, "admin logged in");
    let (id, s) = admin.0.sessions.create(&name, Instant::now());
    let mut r = axum::Json(json!({ "user": s.user, "csrf_token": s.csrf })).into_response();
    let ttl = admin.0.sessions.ttl().as_secs();
    r.headers_mut().insert(header::SET_COOKIE, set_cookie(SESSION_COOKIE, &id, "/", ttl, secure));
    r
}

async fn logout(State(admin): State<Option<Admin>>, req: Request) -> Response {
    let Some(admin) = admin else { return StatusCode::NOT_FOUND.into_response() };
    if let Some(id) = cookie(req.headers(), SESSION_COOKIE) {
        admin.0.sessions.remove(id);
    }
    let mut r = StatusCode::NO_CONTENT.into_response();
    r.headers_mut().insert(header::SET_COOKIE, set_cookie(SESSION_COOKIE, "", "/", 0, secure(&admin, &req)));
    r
}

fn see_other(location: &str) -> Response {
    let mut r = StatusCode::SEE_OTHER.into_response();
    if let Ok(v) = HeaderValue::from_str(location) {
        r.headers_mut().insert(header::LOCATION, v);
    }
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

async fn oidc_start(State(admin): State<Option<Admin>>, req: Request) -> Response {
    let Some(admin) = admin else { return StatusCode::NOT_FOUND.into_response() };
    let Some(oidc) = &admin.0.oidc else { return StatusCode::NOT_FOUND.into_response() };
    let ip = client_ip(&req);
    if let Err(retry) = admin.0.limiter.hit(ip, Instant::now()) {
        return rate_limited(retry);
    }
    match oidc.start(Instant::now()).await {
        Ok((url, state)) => {
            let mut r = see_other(&url);
            let max_age = oidc::PENDING_TTL.as_secs();
            let c = set_cookie(OIDC_COOKIE, &state, OIDC_COOKIE_PATH, max_age, secure(&admin, &req));
            r.headers_mut().insert(header::SET_COOKIE, c);
            r
        }
        Err(e) => {
            tracing::error!(error = %e, "sso sign-in could not start");
            see_other("/login?error=sso_unavailable")
        }
    }
}

async fn oidc_callback(
    State(admin): State<Option<Admin>>,
    Query(q): Query<HashMap<String, String>>,
    req: Request,
) -> Response {
    let Some(admin) = admin else { return StatusCode::NOT_FOUND.into_response() };
    let Some(oidc) = &admin.0.oidc else { return StatusCode::NOT_FOUND.into_response() };
    let ip = client_ip(&req);
    if let Err(retry) = admin.0.limiter.hit(ip, Instant::now()) {
        return rate_limited(retry);
    }
    let secure = secure(&admin, &req);
    let clear = set_cookie(OIDC_COOKIE, "", OIDC_COOKIE_PATH, 0, secure);
    let result = match (q.get("code"), q.get("state")) {
        (Some(code), Some(state)) => oidc.finish(code, state, cookie(req.headers(), OIDC_COOKIE), Instant::now()).await,
        _ => Err(format!("provider returned no code (error: {})", q.get("error").map_or("none", |e| e.as_str()))),
    };
    let mut r = match result {
        Ok(user) => {
            tracing::info!(%ip, %user, "admin logged in via sso");
            let (id, _) = admin.0.sessions.create(&user, Instant::now());
            let mut r = see_other("/");
            let ttl = admin.0.sessions.ttl().as_secs();
            r.headers_mut().append(header::SET_COOKIE, set_cookie(SESSION_COOKIE, &id, "/", ttl, secure));
            r
        }
        Err(e) => {
            tracing::warn!(%ip, error = %e, "sso sign-in refused");
            see_other("/login?error=sso")
        }
    };
    r.headers_mut().append(header::SET_COOKIE, clear);
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_paths_need_an_admin() {
        let get = Method::GET;
        let post = Method::POST;
        for p in [
            "/api/v1/streams",
            "/api/v1/streams/x/cues",
            "/api/v1/recordings",
            "/api/v1/auth/logout",
            "/api",
            "/api/v2/anything",
            "/metrics",
        ] {
            assert!(needs_admin(&get, p, false), "{p}");
        }
        for p in [
            "/healthz",
            "/readyz",
            "/hls/live/index.m3u8",
            "/play/live",
            "/vod/live/1/index.m3u8",
            "/moq/fingerprint",
            "/api/v1/auth/session",
            "/",
            "/streams/live",
            "/assets/index.js",
        ] {
            assert!(!needs_admin(&get, p, false), "{p}");
        }
        assert!(!needs_admin(&post, "/whip/live", false));
        assert!(!needs_admin(&Method::DELETE, "/whep/live/abc", false));
        assert!(!needs_admin(&post, "/api/v1/auth/login", false));
        assert!(!needs_admin(&get, "/metrics", true));
        // Writes to paths nobody claimed default to closed.
        assert!(needs_admin(&post, "/something-new", false));
        // Prefixes are exact: /apix is not /api, /hlsx is not /hls/.
        assert!(!needs_admin(&get, "/apix", false));
        assert!(needs_admin(&post, "/hlsx/a", false));
    }

    #[test]
    fn cookie_parsing() {
        let mut h = HeaderMap::new();
        h.append(header::COOKIE, HeaderValue::from_static("a=1; caudal_session=abc; b=2"));
        assert_eq!(cookie(&h, SESSION_COOKIE), Some("abc"));
        assert_eq!(cookie(&h, "b"), Some("2"));
        assert_eq!(cookie(&h, "caudal"), None);
        let mut h = HeaderMap::new();
        h.append(header::COOKIE, HeaderValue::from_static("x=1"));
        h.append(header::COOKIE, HeaderValue::from_static("caudal_session=zz"));
        assert_eq!(cookie(&h, SESSION_COOKIE), Some("zz"));
    }
}
