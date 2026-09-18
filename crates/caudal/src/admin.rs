//! Wires `[admin]` login (`caudal-admin`) into the server: the startup
//! rule, the gate around the whole HTTP app, and `caudal hash-password`.

use std::io::BufRead;
use std::process::ExitCode;

use caudal_admin::{Admin, Exposure};

use crate::config::Config;

/// Decides whether this server may start and with which gate. `Err` is
/// the refuse-to-start message.
pub fn setup(cfg: &Config) -> Result<Option<Admin>, String> {
    let mut binds = vec![cfg.server.http_bind];
    binds.extend(cfg.tls.bind);
    match caudal_admin::check_exposure(cfg.admin.as_ref(), &binds)? {
        Exposure::Protected => {
            let section = cfg.admin.as_ref().expect("protected implies [admin]");
            let admin = Admin::new(section)?;
            tracing::info!(
                users = section.users.len(),
                sso = section.oidc.is_some(),
                api_tokens = section.api_tokens.len(),
                "admin login required for the UI and management API"
            );
            Ok(admin)
        }
        Exposure::Open => {
            let public = binds.iter().any(|b| !b.ip().is_loopback());
            if public {
                tracing::error!(
                    ?binds,
                    "!!! NO ADMIN LOGIN on a public address ([admin] allow_unauthenticated = true): anyone who can reach this server can manage it"
                );
            } else {
                tracing::warn!(
                    ?binds,
                    "no [admin] login: the UI and management API are open to anyone who can reach these loopback addresses; add [admin] before exposing this server"
                );
            }
            Ok(None)
        }
    }
}

/// Mounts `/api/v1/auth/*` and, with login on, puts the gate in front of
/// every route. Call after every other router is merged.
pub fn wrap(app: axum::Router, admin: Option<Admin>) -> axum::Router {
    let app = app.merge(caudal_admin::router(admin.clone()));
    match admin {
        Some(a) => caudal_admin::protect(app, a),
        None => app,
    }
}

/// `caudal hash-password`: one password on stdin (first line), its
/// argon2id PHC string on stdout.
pub fn hash_password_cmd() -> ExitCode {
    let mut line = String::new();
    if let Err(e) = std::io::stdin().lock().read_line(&mut line) {
        eprintln!("reading stdin: {e}");
        return ExitCode::FAILURE;
    }
    let password = line.trim_end_matches(['\r', '\n']);
    if password.chars().count() < 8 {
        eprintln!("password must be at least 8 characters");
        return ExitCode::FAILURE;
    }
    match caudal_admin::hash_password(password) {
        Ok(phc) => {
            println!("{phc}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use caudal_core::Registry;
    use tower::ServiceExt;

    use super::*;

    const TOKEN: &str = "prometheus-scrape-token";

    fn cfg(toml_src: &str) -> Config {
        toml::from_str(toml_src).unwrap()
    }

    /// The real API and UI routers behind the gate, as `run` builds them.
    fn app() -> axum::Router {
        let phc = caudal_admin::hash_password("hunter2hunter2").unwrap();
        let cfg = cfg(&format!(
            "[server]\nhttp_bind = \"0.0.0.0:8080\"\n[[admin.users]]\nname = \"ana\"\npassword_hash = \"{phc}\"\n[[admin.api_tokens]]\nname = \"prom\"\ntoken_sha256 = \"{}\"\n",
            hex::encode(<sha2::Sha256 as sha2::Digest>::digest(TOKEN))
        ));
        cfg.validate().unwrap();
        let admin = setup(&cfg).unwrap();
        assert!(admin.is_some());
        let state = crate::api::AppState::new(Registry::new());
        wrap(crate::api::router(state).merge(caudal_ui::router()), admin)
    }

    async fn status(app: &axum::Router, req: Request<Body>) -> StatusCode {
        app.clone().oneshot(req).await.unwrap().status()
    }

    fn get(uri: &str) -> Request<Body> {
        Request::get(uri).body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn every_management_route_needs_a_login() {
        let app = app();
        for uri in ["/api/v1/streams", "/api/v1/streams/x", "/api/v1/recordings", "/api/v1/channels", "/metrics"] {
            assert_eq!(status(&app, get(uri)).await, StatusCode::UNAUTHORIZED, "{uri}");
        }
        let cue = Request::post("/api/v1/streams/x/cues").body(Body::from("{}")).unwrap();
        assert_eq!(status(&app, cue).await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn public_routes_stay_public() {
        let app = app();
        assert_eq!(status(&app, get("/healthz")).await, StatusCode::OK);
        assert_eq!(status(&app, get("/readyz")).await, StatusCode::SERVICE_UNAVAILABLE, "public, not ready");
        assert_eq!(status(&app, get("/")).await, StatusCode::OK, "UI bundle");
        assert_eq!(status(&app, get("/streams/live")).await, StatusCode::OK, "UI route");
        assert_eq!(status(&app, get("/api/v1/auth/session")).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn session_cookie_and_api_token_open_the_api() {
        let app = app();
        let login = Request::post("/api/v1/auth/login")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"name":"ana","password":"hunter2hunter2"}"#))
            .unwrap();
        let res = app.clone().oneshot(login).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let sc = res.headers()[header::SET_COOKIE].to_str().unwrap().to_owned();
        let cookie = sc.split(';').next().unwrap().to_owned();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let csrf =
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["csrf_token"].as_str().unwrap().to_owned();

        let with_cookie = |uri: &str| Request::get(uri).header(header::COOKIE, &cookie).body(Body::empty()).unwrap();
        assert_eq!(status(&app, with_cookie("/api/v1/streams")).await, StatusCode::OK);
        assert_eq!(status(&app, with_cookie("/metrics")).await, StatusCode::OK);
        // A write with the cookie reaches the handler only with the CSRF token.
        let cue = |csrf: Option<&str>| {
            let mut b = Request::post("/api/v1/streams/x/cues").header(header::COOKIE, &cookie);
            if let Some(c) = csrf {
                b = b.header("x-csrf-token", c);
            }
            b.body(Body::from(r#"{"kind":"in"}"#)).unwrap()
        };
        assert_eq!(status(&app, cue(None)).await, StatusCode::FORBIDDEN);
        assert_eq!(status(&app, cue(Some(&csrf))).await, StatusCode::NOT_FOUND, "handler ran: no such stream");

        let bearer = |uri: &str| {
            Request::get(uri).header(header::AUTHORIZATION, format!("Bearer {TOKEN}")).body(Body::empty()).unwrap()
        };
        assert_eq!(status(&app, bearer("/api/v1/streams")).await, StatusCode::OK);
        assert_eq!(status(&app, bearer("/metrics")).await, StatusCode::OK);
    }

    #[test]
    fn refuses_to_start_open_on_a_public_address() {
        let err = setup(&cfg("[server]\nhttp_bind = \"0.0.0.0:8080\"\n")).unwrap_err();
        assert!(err.contains("refusing to start"), "{err}");
        assert!(setup(&cfg("[server]\nhttp_bind = \"127.0.0.1:8080\"\n")).unwrap().is_none());
        // HTTPS on a public address counts.
        let tls =
            "[server]\nhttp_bind = \"127.0.0.1:8080\"\n[tls]\nbind = \"0.0.0.0:8443\"\ncert = \"c\"\nkey = \"k\"\n";
        assert!(setup(&cfg(tls)).is_err());
        let allowed = "[server]\nhttp_bind = \"0.0.0.0:8080\"\n[admin]\nallow_unauthenticated = true\n";
        assert!(setup(&cfg(allowed)).unwrap().is_none());
    }
}
