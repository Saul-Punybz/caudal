//! The gate end to end, in process: password login, sessions, CSRF, API
//! tokens, rate limiting, and OIDC against a tiny mock issuer.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Form, State};
use axum::http::{Method, Request, StatusCode, header};
use axum::routing::{get, post};
use caudal_admin::{Admin, AdminSection, ApiTokenEntry, OidcSection, UserEntry};
use sha2::Digest;
use tower::ServiceExt;

const TOKEN: &str = "automation-token-0123456789";

fn section() -> AdminSection {
    AdminSection {
        users: vec![UserEntry { name: "ana".into(), password_hash: caudal_admin::hash_password("hunter2!").unwrap() }],
        api_tokens: vec![ApiTokenEntry { name: "ci".into(), token_sha256: hex::encode(sha2::Sha256::digest(TOKEN)) }],
        ..Default::default()
    }
}

/// The admin routes plus a stand-in management route and a public one.
fn app(section: &AdminSection) -> Router {
    let admin = Admin::new(section).unwrap().expect("login enabled");
    let inner = Router::new()
        .route("/api/v1/streams", get(|| async { "[]" }).post(|| async { "posted" }))
        .route("/healthz", get(|| async { "ok" }))
        .route("/hls/{name}/{file}", get(|| async { "#EXTM3U" }))
        .route("/metrics", get(|| async { "caudal_streams 0" }))
        .merge(caudal_admin::router(Some(admin.clone())));
    caudal_admin::protect(inner, admin)
}

struct Res {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: serde_json::Value,
}

async fn call(app: &Router, method: Method, uri: &str, headers: &[(&str, &str)], body: &str) -> Res {
    call_from(app, [203, 0, 113, 9], method, uri, headers, body).await
}

/// A fresh client address per SSO round trip, so the login rate limit
/// (10 per minute per address) does not trip across many sign-ins.
fn next_ip() -> [u8; 4] {
    static N: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(1);
    [198, 51, 100, N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)]
}

async fn call_from(app: &Router, ip: [u8; 4], method: Method, uri: &str, headers: &[(&str, &str)], body: &str) -> Res {
    let mut b = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let mut req = b.body(Body::from(body.to_owned())).unwrap();
    req.extensions_mut().insert(ConnectInfo(SocketAddr::from((ip, 5000))));
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let body = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| serde_json::Value::String(String::from_utf8_lossy(&bytes).into()));
    Res { status, headers, body }
}

fn set_cookie<'a>(res: &'a Res, name: &str) -> Option<&'a str> {
    res.headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with(&format!("{name}=")))
}

fn cookie_value(set_cookie: &str) -> String {
    set_cookie.split(';').next().unwrap().split_once('=').unwrap().1.to_owned()
}

async fn login(app: &Router) -> (String, String) {
    let res = call(
        app,
        Method::POST,
        "/api/v1/auth/login",
        &[("content-type", "application/json")],
        r#"{"name":"ana","password":"hunter2!"}"#,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{:?}", res.body);
    let sc = set_cookie(&res, "caudal_session").unwrap();
    assert!(sc.contains("HttpOnly") && sc.contains("SameSite=Lax") && sc.contains("Path=/"), "{sc}");
    assert!(!sc.contains("Secure"), "plain HTTP without secure_cookies");
    (cookie_value(sc), res.body["csrf_token"].as_str().unwrap().to_owned())
}

#[tokio::test]
async fn management_routes_need_a_login_and_public_ones_do_not() {
    let app = app(&section());
    let res = call(&app, Method::GET, "/api/v1/streams", &[], "").await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
    assert_eq!(res.headers.get("x-caudal-login").unwrap(), "required");
    assert_eq!(call(&app, Method::GET, "/metrics", &[], "").await.status, StatusCode::UNAUTHORIZED);
    // Unknown /api paths are closed too, not a 404 that maps the surface.
    assert_eq!(call(&app, Method::GET, "/api/v1/nope", &[], "").await.status, StatusCode::UNAUTHORIZED);

    assert_eq!(call(&app, Method::GET, "/healthz", &[], "").await.status, StatusCode::OK);
    assert_eq!(call(&app, Method::GET, "/hls/live/index.m3u8", &[], "").await.status, StatusCode::OK);
    let s = call(&app, Method::GET, "/api/v1/auth/session", &[], "").await;
    assert_eq!(s.status, StatusCode::OK);
    assert_eq!(s.body["required"], true);
    assert_eq!(s.body["authenticated"], false);
    assert_eq!(s.body["password"], true);
    assert_eq!(s.body["oidc"], false);
}

#[tokio::test]
async fn session_cookie_opens_the_api_and_csrf_guards_writes() {
    let app = app(&section());
    let (sid, csrf) = login(&app).await;
    let cookie = format!("caudal_session={sid}");

    let res = call(&app, Method::GET, "/api/v1/streams", &[("cookie", &cookie)], "").await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(call(&app, Method::GET, "/metrics", &[("cookie", &cookie)], "").await.status, StatusCode::OK);

    // Writes: the cookie alone is not enough.
    let res = call(&app, Method::POST, "/api/v1/streams", &[("cookie", &cookie)], "").await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = call(&app, Method::POST, "/api/v1/streams", &[("cookie", &cookie), ("x-csrf-token", "wrong")], "").await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = call(&app, Method::POST, "/api/v1/streams", &[("cookie", &cookie), ("x-csrf-token", &csrf)], "").await;
    assert_eq!(res.status, StatusCode::OK);

    let s = call(&app, Method::GET, "/api/v1/auth/session", &[("cookie", &cookie)], "").await;
    assert_eq!(s.body["authenticated"], true);
    assert_eq!(s.body["user"], "ana");
    assert_eq!(s.body["csrf_token"], csrf.as_str());

    // Logout is a write: it needs the token, then the session is gone.
    let res = call(&app, Method::POST, "/api/v1/auth/logout", &[("cookie", &cookie)], "").await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res =
        call(&app, Method::POST, "/api/v1/auth/logout", &[("cookie", &cookie), ("x-csrf-token", &csrf)], "").await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    assert!(set_cookie(&res, "caudal_session").unwrap().contains("Max-Age=0"));
    let res = call(&app, Method::GET, "/api/v1/streams", &[("cookie", &cookie)], "").await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn api_tokens_work_without_csrf_and_bad_ones_do_not() {
    let app = app(&section());
    let auth = format!("Bearer {TOKEN}");
    assert_eq!(
        call(&app, Method::GET, "/api/v1/streams", &[("authorization", &auth)], "").await.status,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, Method::POST, "/api/v1/streams", &[("authorization", &auth)], "").await.status,
        StatusCode::OK
    );
    assert_eq!(call(&app, Method::GET, "/metrics", &[("authorization", &auth)], "").await.status, StatusCode::OK);
    let bad = call(&app, Method::GET, "/api/v1/streams", &[("authorization", "Bearer nope")], "").await;
    assert_eq!(bad.status, StatusCode::UNAUTHORIZED);
    // The raw hash is not the token.
    let hash = format!("Bearer {}", hex::encode(sha2::Sha256::digest(TOKEN)));
    assert_eq!(
        call(&app, Method::GET, "/api/v1/streams", &[("authorization", &hash)], "").await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn bad_logins_are_refused_and_rate_limited() {
    let app = app(&section());
    let json = [("content-type", "application/json")];
    let wrong = call(&app, Method::POST, "/api/v1/auth/login", &json, r#"{"name":"ana","password":"x"}"#).await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert!(set_cookie(&wrong, "caudal_session").is_none());
    let nobody = call(&app, Method::POST, "/api/v1/auth/login", &json, r#"{"name":"bob","password":"x"}"#).await;
    assert_eq!(nobody.status, StatusCode::UNAUTHORIZED);
    assert_eq!(nobody.body, wrong.body, "unknown users look the same as wrong passwords");
    // Form posts (cross-site forms) are not accepted.
    let form = call(
        &app,
        Method::POST,
        "/api/v1/auth/login",
        &[("content-type", "application/x-www-form-urlencoded")],
        "name=ana&password=hunter2!",
    )
    .await;
    assert_eq!(form.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    // 2 attempts so far; the 10th is the last one allowed in the window.
    for _ in 0..8 {
        call(&app, Method::POST, "/api/v1/auth/login", &json, r#"{"name":"ana","password":"x"}"#).await;
    }
    let limited =
        call(&app, Method::POST, "/api/v1/auth/login", &json, r#"{"name":"ana","password":"hunter2!"}"#).await;
    assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(limited.headers.get(header::RETRY_AFTER).is_some());
}

#[tokio::test]
async fn secure_cookie_over_tls() {
    let s = AdminSection { secure_cookies: true, ..section() };
    let app = app(&s);
    let res = call(
        &app,
        Method::POST,
        "/api/v1/auth/login",
        &[("content-type", "application/json")],
        r#"{"name":"ana","password":"hunter2!"}"#,
    )
    .await;
    assert!(set_cookie(&res, "caudal_session").unwrap().ends_with("; Secure"));
}

#[tokio::test]
async fn without_login_configured_the_ui_is_told_so() {
    let app = caudal_admin::router(None);
    let s = call(&app, Method::GET, "/api/v1/auth/session", &[], "").await;
    assert_eq!(s.body["required"], false);
    let res = call(&app, Method::POST, "/api/v1/auth/login", &[("content-type", "application/json")], "{}").await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
}

// ---- OIDC against a mock issuer ----

#[derive(Default, Clone)]
struct Mock {
    issuer: String,
    /// Captured from the authorize URL by the test (the "browser").
    nonce: String,
    challenge: String,
    /// What the next ID token says.
    email: String,
    email_verified: bool,
    groups: Vec<String>,
    aud: String,
    nonce_override: Option<String>,
    /// PKCE verifier the token endpoint received, checked against the challenge.
    pkce_ok: Option<bool>,
}

struct Issuer {
    state: Arc<Mutex<Mock>>,
    key_der: Vec<u8>,
}

async fn discovery(State(i): State<Arc<Issuer>>) -> axum::Json<serde_json::Value> {
    let iss = i.state.lock().unwrap().issuer.clone();
    axum::Json(serde_json::json!({
        "issuer": iss,
        "authorization_endpoint": format!("{iss}/authorize"),
        "token_endpoint": format!("{iss}/token"),
        "jwks_uri": format!("{iss}/jwks"),
        "response_types_supported": ["code"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["ES256"],
    }))
}

async fn token(State(i): State<Arc<Issuer>>, Form(f): Form<HashMap<String, String>>) -> axum::response::Response {
    use axum::response::IntoResponse;
    use base64::Engine;
    let mut m = i.state.lock().unwrap();
    if f.get("code").map(String::as_str) != Some("good-code") {
        return (StatusCode::BAD_REQUEST, axum::Json(serde_json::json!({"error": "invalid_grant"}))).into_response();
    }
    let verifier = f.get("code_verifier").cloned().unwrap_or_default();
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(&verifier));
    m.pkce_ok = Some(challenge == m.challenge);
    if challenge != m.challenge {
        return (StatusCode::BAD_REQUEST, axum::Json(serde_json::json!({"error": "invalid_grant"}))).into_response();
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let claims = serde_json::json!({
        "iss": m.issuer, "sub": "user-1", "aud": m.aud, "iat": now, "exp": now + 300,
        "nonce": m.nonce_override.clone().unwrap_or(m.nonce.clone()),
        "email": m.email, "email_verified": m.email_verified, "groups": m.groups,
    });
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
    header.kid = Some("k1".into());
    let jwt = jsonwebtoken::encode(&header, &claims, &jsonwebtoken::EncodingKey::from_ec_der(&i.key_der)).unwrap();
    axum::Json(serde_json::json!({
        "access_token": "at", "token_type": "Bearer", "expires_in": 3600, "id_token": jwt,
    }))
    .into_response()
}

/// Starts the mock issuer on an ephemeral port (bound, never probed).
async fn mock_issuer() -> Arc<Mutex<Mock>> {
    use base64::Engine;
    let key = rcgen::KeyPair::generate().unwrap();
    let raw = key.public_key_raw();
    let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
    let jwks = serde_json::json!({"keys": [{
        "kty": "EC", "crv": "P-256", "x": b64(&raw[1..33]), "y": b64(&raw[33..65]),
        "kid": "k1", "use": "sig", "alg": "ES256",
    }]});
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    let state = Arc::new(Mutex::new(Mock {
        issuer,
        email: "ana@example.com".into(),
        email_verified: true,
        aud: "caudal".into(),
        ..Default::default()
    }));
    let i = Arc::new(Issuer { state: state.clone(), key_der: key.serialize_der() });
    let app = Router::new()
        .route("/.well-known/openid-configuration", get(discovery))
        .route("/jwks", get(move || async move { axum::Json(jwks) }))
        .route("/token", post(token))
        .with_state(i);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    state
}

fn oidc_app(mock: &Arc<Mutex<Mock>>, allowed_emails: &[&str], allowed_groups: &[&str]) -> Router {
    let s = AdminSection {
        oidc: Some(OidcSection {
            issuer: mock.lock().unwrap().issuer.clone(),
            client_id: "caudal".into(),
            client_secret: Some("shh".into()),
            redirect_url: "http://127.0.0.1:8080/api/v1/auth/oidc/callback".into(),
            allowed_emails: allowed_emails.iter().map(|s| s.to_string()).collect(),
            allowed_groups: allowed_groups.iter().map(|s| s.to_string()).collect(),
            scopes: vec![],
        }),
        ..Default::default()
    };
    app(&s)
}

/// Plays the browser: start, "log in" at the provider, come back. Returns
/// the callback response.
async fn sso(app: &Router, mock: &Arc<Mutex<Mock>>, send_cookie: bool) -> Res {
    let ip = next_ip();
    let start = call_from(app, ip, Method::GET, "/api/v1/auth/oidc/start", &[], "").await;
    assert_eq!(start.status, StatusCode::SEE_OTHER, "{:?}", start.body);
    let location = start.headers.get(header::LOCATION).unwrap().to_str().unwrap();
    let url = openidconnect::url::Url::parse(location).unwrap();
    assert!(location.starts_with(&format!("{}/authorize?", mock.lock().unwrap().issuer)), "{location}");
    let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(q["client_id"], "caudal");
    assert!(q["scope"].contains("openid"));
    {
        let mut m = mock.lock().unwrap();
        m.nonce = q["nonce"].clone();
        m.challenge = q["code_challenge"].clone();
    }
    let sc = set_cookie(&start, "caudal_oidc").unwrap();
    assert!(sc.contains("Path=/api/v1/auth/oidc") && sc.contains("HttpOnly"), "{sc}");
    let state_cookie = format!("caudal_oidc={}", cookie_value(sc));
    let uri = format!("/api/v1/auth/oidc/callback?code=good-code&state={}", q["state"]);
    let headers: Vec<(&str, &str)> = if send_cookie { vec![("cookie", &state_cookie)] } else { vec![] };
    call_from(app, ip, Method::GET, &uri, &headers, "").await
}

fn location(res: &Res) -> &str {
    res.headers.get(header::LOCATION).unwrap().to_str().unwrap()
}

#[tokio::test]
async fn oidc_code_flow_with_pkce_logs_in() {
    let mock = mock_issuer().await;
    let app = oidc_app(&mock, &["Ana@Example.com"], &[]);
    let s = call(&app, Method::GET, "/api/v1/auth/session", &[], "").await;
    assert_eq!(s.body["oidc"], true);
    assert_eq!(s.body["password"], false);

    let res = sso(&app, &mock, true).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert_eq!(location(&res), "/");
    assert_eq!(mock.lock().unwrap().pkce_ok, Some(true));
    let sid = cookie_value(set_cookie(&res, "caudal_session").expect("session cookie"));
    let cookie = format!("caudal_session={sid}");
    assert_eq!(call(&app, Method::GET, "/api/v1/streams", &[("cookie", &cookie)], "").await.status, StatusCode::OK);
    let s = call(&app, Method::GET, "/api/v1/auth/session", &[("cookie", &cookie)], "").await;
    assert_eq!(s.body["user"], "ana@example.com");
}

#[tokio::test]
async fn oidc_refuses_bad_callbacks_and_tokens() {
    let mock = mock_issuer().await;
    let app = oidc_app(&mock, &["ana@example.com"], &["ops"]);
    let refused = |res: &Res| {
        assert_eq!(res.status, StatusCode::SEE_OTHER);
        assert_eq!(location(res), "/login?error=sso");
        assert!(set_cookie(res, "caudal_session").is_none());
    };

    // No state cookie: a callback started in another browser (login CSRF).
    refused(&sso(&app, &mock, false).await);

    // Nonce from another sign-in (replayed ID token).
    mock.lock().unwrap().nonce_override = Some("someone-elses-nonce".into());
    refused(&sso(&app, &mock, true).await);
    mock.lock().unwrap().nonce_override = None;

    // Token minted for another client.
    mock.lock().unwrap().aud = "other-app".into();
    refused(&sso(&app, &mock, true).await);
    mock.lock().unwrap().aud = "caudal".into();

    // Allowed email, but not verified by the provider.
    mock.lock().unwrap().email_verified = false;
    refused(&sso(&app, &mock, true).await);

    // Unverified, but in an allowed group: in.
    mock.lock().unwrap().groups = vec!["ops".into()];
    let ok = sso(&app, &mock, true).await;
    assert_eq!(location(&ok), "/");

    // Neither email nor group matches.
    {
        let mut m = mock.lock().unwrap();
        m.email = "eve@example.com".into();
        m.email_verified = true;
        m.groups = vec!["guests".into()];
    }
    refused(&sso(&app, &mock, true).await);
}

#[tokio::test]
async fn oidc_state_is_single_use() {
    let mock = mock_issuer().await;
    let app = oidc_app(&mock, &["ana@example.com"], &[]);
    let start = call(&app, Method::GET, "/api/v1/auth/oidc/start", &[], "").await;
    let url = openidconnect::url::Url::parse(location(&start)).unwrap();
    let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
    {
        let mut m = mock.lock().unwrap();
        m.nonce = q["nonce"].clone();
        m.challenge = q["code_challenge"].clone();
    }
    let cookie = format!("caudal_oidc={}", q["state"]);
    let uri = format!("/api/v1/auth/oidc/callback?code=good-code&state={}", q["state"]);
    let first = call(&app, Method::GET, &uri, &[("cookie", &cookie)], "").await;
    assert_eq!(location(&first), "/");
    let again = call(&app, Method::GET, &uri, &[("cookie", &cookie)], "").await;
    assert_eq!(location(&again), "/login?error=sso");
}

#[tokio::test]
async fn oidc_provider_down_does_not_break_anything_else() {
    // Port 1 (tcpmux): nothing listens there, so discovery fails.
    let mock = Arc::new(Mutex::new(Mock { issuer: "http://127.0.0.1:1".into(), ..Default::default() }));
    let app = oidc_app(&mock, &["ana@example.com"], &[]);
    let start = call(&app, Method::GET, "/api/v1/auth/oidc/start", &[], "").await;
    assert_eq!(location(&start), "/login?error=sso_unavailable");
    assert_eq!(call(&app, Method::GET, "/healthz", &[], "").await.status, StatusCode::OK);
}
