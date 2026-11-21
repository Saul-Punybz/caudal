//! Hot config reload: `POST /api/v1/config/reload` and, on Unix, SIGHUP.
//! Both re-read the TOML file the process was started with, validate it
//! (an invalid file is rejected and the running config keeps going — see
//! `config::load`), and hand it to [`crate::subsystems::Supervisor::reload`]
//! to apply the smallest correct change per section.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::config_edit::{self, EditError, Kind};

use crate::subsystems::{ReloadReport, Supervisor};

/// State for the reload route: the supervisor to apply changes to, and the
/// config file path to re-read on `POST` or SIGHUP.
#[derive(Clone)]
pub struct ReloadState {
    pub supervisor: Arc<Supervisor>,
    /// `None` when the process started with no `--config` and no default
    /// `caudal.toml` on disk: there is nothing to re-read, so a reload is
    /// refused rather than silently reapplying built-in defaults.
    pub config_path: Option<PathBuf>,
    /// One config edit at a time: read, change, validate, write, apply.
    pub edits: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Debug, Serialize)]
struct ReloadResponse {
    applied: Vec<String>,
    restarted: Vec<String>,
    requires_restart: Vec<String>,
}

impl From<ReloadReport> for ReloadResponse {
    fn from(r: ReloadReport) -> Self {
        Self { applied: r.applied, restarted: r.restarted, requires_restart: r.requires_restart }
    }
}

/// Re-reads and validates the config file, then applies it. Shared by the
/// API and SIGHUP so the two paths can never drift apart.
fn reload_from_disk(state: &ReloadState) -> Result<ReloadReport, String> {
    let path = state.config_path.as_deref().ok_or("no --config file to reload from")?;
    let cfg = crate::config::load(path)?;
    state.supervisor.reload(cfg)
}

async fn reload_config(State(state): State<ReloadState>) -> Response {
    match reload_from_disk(&state) {
        Ok(report) => {
            tracing::info!(
                applied = ?report.applied,
                restarted = ?report.restarted,
                requires_restart = ?report.requires_restart,
                "config reloaded over the api",
            );
            (StatusCode::OK, Json(ReloadResponse::from(report))).into_response()
        }
        Err(e) => {
            tracing::warn!(error = %e, "config reload rejected; the running config is unchanged");
            (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": e }))).into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
struct EntryBody {
    stream: String,
    url: String,
    /// `whip://` pushes only: the endpoint's bearer token.
    #[serde(default)]
    token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RemoveQuery {
    /// Only the entry with this url; absent: every one of the stream.
    url: Option<String>,
}

fn error(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": msg.into() }))).into_response()
}

/// Read the file, apply `edit`, validate, write atomically, reload. The
/// running config and the file only change together, and only when valid.
async fn edit_config(
    state: &ReloadState,
    what: &str,
    edit: impl FnOnce(&str) -> Result<String, EditError>,
) -> Response {
    let Some(path) = state.config_path.clone() else {
        return error(StatusCode::CONFLICT, "the server runs without a --config file; there is nothing to edit");
    };
    let _one_at_a_time = state.edits.lock().await;
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, format!("reading {}: {e}", path.display())),
    };
    let new_text = match edit(&text) {
        Ok(t) => t,
        Err(EditError::Exists) => return error(StatusCode::CONFLICT, "that entry already exists"),
        Err(EditError::NotFound) => return error(StatusCode::NOT_FOUND, "no such entry"),
        Err(EditError::Invalid(e)) => return error(StatusCode::BAD_REQUEST, e),
    };
    let cfg = match config_edit::check(&new_text) {
        Ok(c) => c,
        Err(e) => return error(StatusCode::BAD_REQUEST, e),
    };
    if let Err(e) = config_edit::write_atomic(&path, &new_text) {
        return error(StatusCode::INTERNAL_SERVER_ERROR, format!("writing {}: {e}", path.display()));
    }
    match state.supervisor.reload(cfg) {
        Ok(report) => {
            tracing::info!(what, applied = ?report.applied, "config edited over the api");
            (StatusCode::OK, Json(ReloadResponse::from(report))).into_response()
        }
        // Validated above, so this means the file on disk changed under us
        // between our write and the apply; report it, the file is written.
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, format!("written but not applied: {e}")),
    }
}

fn current(state: &ReloadState) -> crate::config::Config {
    state.supervisor.current_config()
}

async fn list_pulls(State(state): State<ReloadState>) -> Json<Vec<config_edit::EntryJson>> {
    Json(config_edit::list(&current(&state), &Kind::PULLS))
}

async fn list_pushes(State(state): State<ReloadState>) -> Json<Vec<config_edit::EntryJson>> {
    let cfg = current(&state);
    let mut rows = config_edit::list(&cfg, &Kind::PUSHES);
    rows.extend(config_edit::list_auto_pushes(&cfg));
    Json(rows)
}

async fn list_auto_pushes(State(state): State<ReloadState>) -> Json<Vec<crate::auto_push::RuleJson>> {
    Json(state.supervisor.auto_pushes())
}

/// `POST /api/v1/auto-pushes/{index}/enable` or `/disable`: a runtime
/// override of the rule's `enabled`, not written to the file. Kept across
/// reloads that leave the rule unchanged; lost on restart or when the rule
/// is edited. `200` with the rule as `GET` shows it, `404` for no such rule.
async fn toggle_auto_push(State(state): State<ReloadState>, Path((index, action)): Path<(usize, String)>) -> Response {
    let enabled = match action.as_str() {
        "enable" => true,
        "disable" => false,
        _ => return error(StatusCode::NOT_FOUND, "expected /enable or /disable"),
    };
    if !state.supervisor.set_auto_push_enabled(index, enabled) {
        return error(StatusCode::NOT_FOUND, format!("no [[push.auto]] rule {index}"));
    }
    tracing::info!(rule = index, enabled, "auto push rule toggled over the api");
    match state.supervisor.auto_pushes().into_iter().find(|r| r.index == index) {
        Some(rule) => (StatusCode::OK, Json(rule)).into_response(),
        None => error(StatusCode::NOT_FOUND, format!("no [[push.auto]] rule {index}")),
    }
}

async fn add_pull(State(state): State<ReloadState>, Json(b): Json<EntryBody>) -> Response {
    let kind = match Kind::for_pull(&b.url) {
        Ok(k) => k,
        Err(e) => return error(StatusCode::BAD_REQUEST, e),
    };
    edit_config(&state, "add pull", |t| config_edit::add(t, kind, &b.stream, &b.url)).await
}

async fn add_push(State(state): State<ReloadState>, Json(b): Json<EntryBody>) -> Response {
    let kind = match Kind::for_push(&b.url) {
        Ok(k) => k,
        Err(e) => return error(StatusCode::BAD_REQUEST, e),
    };
    edit_config(&state, "add push", |t| config_edit::add_with(t, kind, &b.stream, &b.url, b.token.as_deref())).await
}

/// Removes from every kind in `kinds`; `NotFound` only when none matched.
fn remove_any(text: &str, kinds: &[Kind], stream: &str, url: Option<&str>) -> Result<String, EditError> {
    let mut text = text.to_owned();
    let mut found = false;
    for &k in kinds {
        match config_edit::remove(&text, k, stream, url) {
            Ok((t, _)) => {
                text = t;
                found = true;
            }
            Err(EditError::NotFound) => {}
            Err(e) => return Err(e),
        }
    }
    if found { Ok(text) } else { Err(EditError::NotFound) }
}

async fn remove_pull(
    State(state): State<ReloadState>,
    Path(stream): Path<String>,
    Query(q): Query<RemoveQuery>,
) -> Response {
    edit_config(&state, "remove pull", |t| remove_any(t, &Kind::PULLS, &stream, q.url.as_deref())).await
}

async fn remove_push(
    State(state): State<ReloadState>,
    Path(stream): Path<String>,
    Query(q): Query<RemoveQuery>,
) -> Response {
    edit_config(&state, "remove push", |t| remove_any(t, &Kind::PUSHES, &stream, q.url.as_deref())).await
}

/// `POST /api/v1/config/reload` → `200 { applied, restarted,
/// requires_restart }`, or `400 { error }` when the file is invalid or
/// missing.
pub fn router(state: ReloadState) -> Router {
    Router::new()
        .route("/api/v1/config/reload", post(reload_config))
        .route("/api/v1/pulls", get(list_pulls).post(add_pull))
        .route("/api/v1/pulls/{stream}", delete(remove_pull))
        .route("/api/v1/pushes", get(list_pushes).post(add_push))
        .route("/api/v1/pushes/{stream}", delete(remove_push))
        .route("/api/v1/auto-pushes", get(list_auto_pushes))
        .route("/api/v1/auto-pushes/{index}/{action}", post(toggle_auto_push))
        .with_state(state)
}

/// Reloads on every SIGHUP until the process exits. A no-op spawn on
/// non-Unix targets (there's no SIGHUP there); `POST
/// /api/v1/config/reload` still works everywhere.
pub fn spawn_sighup(state: ReloadState) {
    #[cfg(unix)]
    {
        tokio::spawn(async move {
            let mut sig = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "failed to install a SIGHUP handler; POST /api/v1/config/reload still works",
                    );
                    return;
                }
            };
            loop {
                sig.recv().await;
                tracing::info!("received sighup: reloading config");
                match reload_from_disk(&state) {
                    Ok(report) => tracing::info!(
                        applied = ?report.applied,
                        restarted = ?report.restarted,
                        requires_restart = ?report.requires_restart,
                        "config reloaded over sighup",
                    ),
                    Err(e) => tracing::warn!(error = %e, "config reload rejected; the running config is unchanged"),
                }
            }
        });
    }
    #[cfg(not(unix))]
    {
        let _ = state;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::subsystems::Supervisor;

    /// Every bind on port 0 (OS-assigned, never probed or read back — the
    /// supervisor never needs to connect to itself), so this never races
    /// another test or another agent's server on this shared machine.
    fn port0_config() -> Config {
        let mut cfg = Config::default();
        cfg.rtmp.bind = "127.0.0.1:0".parse().unwrap();
        cfg.srt.bind = "127.0.0.1:0".parse().unwrap();
        cfg
    }

    #[tokio::test]
    async fn reload_without_a_config_path_is_refused_not_ignored() {
        let registry = caudal_core::Registry::new();
        let started = Supervisor::start(&port0_config(), registry);
        let state = ReloadState { supervisor: started.supervisor, config_path: None, edits: Default::default() };
        let err = reload_from_disk(&state).unwrap_err();
        assert!(err.contains("--config"), "{err}");
    }

    #[tokio::test]
    async fn reload_from_disk_rejects_an_invalid_file_and_keeps_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("caudal.toml");
        std::fs::write(&path, "[server]\nhttp_bnid = 1\n").unwrap();

        let registry = caudal_core::Registry::new();
        let started = Supervisor::start(&port0_config(), registry);
        let state = ReloadState { supervisor: started.supervisor, config_path: Some(path), edits: Default::default() };
        let err = reload_from_disk(&state).unwrap_err();
        assert!(err.contains("http_bnid"), "{err}");
    }

    async fn call(app: &Router, method: &str, uri: &str, body: Option<&str>) -> (StatusCode, serde_json::Value) {
        use tower::ServiceExt;
        let mut req = axum::http::Request::builder().method(method).uri(uri);
        if body.is_some() {
            req = req.header("content-type", "application/json");
        }
        let req = req.body(axum::body::Body::from(body.unwrap_or("").to_owned())).unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    /// Add, list and remove pushes and pulls over the API: the file keeps the
    /// operator's comments, invalid edits leave it untouched, and the
    /// running config follows the file.
    #[tokio::test]
    async fn pulls_and_pushes_edit_the_file_and_apply() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("caudal.toml");
        let original = "# production box\n[rtmp]\nbind = \"127.0.0.1:0\"\n\n[srt]\nbind = \"127.0.0.1:0\" # SRT in\n";
        std::fs::write(&path, original).unwrap();
        let cfg = crate::config::load(&path).unwrap();
        let started = Supervisor::start(&cfg, caudal_core::Registry::new());
        let supervisor = started.supervisor.clone();
        let app = router(ReloadState {
            supervisor: started.supervisor,
            config_path: Some(path.clone()),
            edits: Default::default(),
        });

        let (st, body) =
            call(&app, "POST", "/api/v1/pushes", Some(r#"{"stream":"show","url":"srt://127.0.0.1:9"}"#)).await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert!(body["applied"].as_array().unwrap().iter().any(|a| a == "srt.push"), "{body}");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# production box\n") && text.contains("# SRT in"), "comments kept:\n{text}");
        assert_eq!(supervisor.current_config().srt.push.len(), 1, "applied to the running server");

        let (st, _) =
            call(&app, "POST", "/api/v1/pushes", Some(r#"{"stream":"show","url":"rtmps://live.example/app/k"}"#)).await;
        assert_eq!(st, StatusCode::OK);
        let (_, list) = call(&app, "GET", "/api/v1/pushes", None).await;
        let sections: Vec<&str> = list.as_array().unwrap().iter().map(|e| e["section"].as_str().unwrap()).collect();
        assert_eq!(sections, ["[[srt.push]]", "[[restream]]"]);

        // Refusals: duplicate, a UDP push to a hostname, invalid stream name. None of
        // them touches the file.
        let before = std::fs::read_to_string(&path).unwrap();
        let dup = r#"{"stream":"show","url":"srt://127.0.0.1:9"}"#;
        assert_eq!(call(&app, "POST", "/api/v1/pushes", Some(dup)).await.0, StatusCode::CONFLICT);
        // udp:// needs an IP address: refused by the config check.
        let udp = r#"{"stream":"show","url":"udp://mux.local:5000"}"#;
        assert_eq!(call(&app, "POST", "/api/v1/pushes", Some(udp)).await.0, StatusCode::BAD_REQUEST);
        let (st, body) =
            call(&app, "POST", "/api/v1/pulls", Some(r#"{"stream":"bad name","url":"rtsp://10.0.0.5/x"}"#)).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before, "a refused edit never writes");

        let (st, _) =
            call(&app, "POST", "/api/v1/pulls", Some(r#"{"stream":"cam","url":"rtsp://127.0.0.1:9/x"}"#)).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(supervisor.current_config().rtsp.pull.len(), 1);
        assert_eq!(call(&app, "DELETE", "/api/v1/pulls/cam", None).await.0, StatusCode::OK);
        assert_eq!(call(&app, "DELETE", "/api/v1/pulls/cam", None).await.0, StatusCode::NOT_FOUND);
        assert!(supervisor.current_config().rtsp.pull.is_empty());

        // Remove only one of the stream's pushes, then the rest.
        let uri = "/api/v1/pushes/show?url=rtmps://live.example/app/k";
        assert_eq!(call(&app, "DELETE", uri, None).await.0, StatusCode::OK);
        assert_eq!(supervisor.current_config().restream.len(), 0);
        assert_eq!(supervisor.current_config().srt.push.len(), 1);
        assert_eq!(call(&app, "DELETE", "/api/v1/pushes/show", None).await.0, StatusCode::OK);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(crate::config_edit::check(&text).unwrap().srt.push.is_empty());
        assert!(text.starts_with("# production box\n"));
    }

    /// A WHIP push over the API: written to `[[webrtc.push]]` with its
    /// token, applied live (not a `[webrtc]` restart), listed masked.
    #[tokio::test]
    async fn whip_push_over_the_api() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("caudal.toml");
        std::fs::write(&path, "[rtmp]\nbind = \"127.0.0.1:0\"\n[srt]\nbind = \"127.0.0.1:0\"\n").unwrap();
        let cfg = crate::config::load(&path).unwrap();
        let started = Supervisor::start(&cfg, caudal_core::Registry::new());
        let supervisor = started.supervisor.clone();
        let app = router(ReloadState {
            supervisor: started.supervisor,
            config_path: Some(path.clone()),
            edits: Default::default(),
        });
        let add = r#"{"stream":"main","url":"whip://cf.example/0123456789abcdef0123/webRTC/publish","token":"s3cr3t"}"#;
        let (st, body) = call(&app, "POST", "/api/v1/pushes", Some(add)).await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert!(body["applied"].as_array().unwrap().iter().any(|a| a == "webrtc.push"), "{body}");
        assert!(body["requires_restart"].as_array().unwrap().is_empty(), "{body}");
        assert_eq!(supervisor.current_config().webrtc.push[0].token.as_deref(), Some("s3cr3t"));
        let (_, list) = call(&app, "GET", "/api/v1/pushes", None).await;
        assert_eq!(
            list,
            serde_json::json!([{"stream": "main", "url": "whip://cf.example/****/webRTC/publish", "section": "[[webrtc.push]]"}])
        );
        let tok = r#"{"stream":"main","url":"srt://127.0.0.1:9","token":"x"}"#;
        assert_eq!(call(&app, "POST", "/api/v1/pushes", Some(tok)).await.0, StatusCode::BAD_REQUEST);
        assert_eq!(call(&app, "DELETE", "/api/v1/pushes/main", None).await.0, StatusCode::OK);
        assert!(supervisor.current_config().webrtc.push.is_empty());
    }

    /// `[[push.auto]]` arrives by reload, shows in `GET /api/v1/pushes` in
    /// the same row shape (masked), and `GET /api/v1/auto-pushes` lists the
    /// push it runs for a live matching stream.
    #[tokio::test]
    async fn auto_push_rules_reload_and_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("caudal.toml");
        let base = "[rtmp]\nbind = \"127.0.0.1:0\"\n[srt]\nbind = \"127.0.0.1:0\"\n";
        std::fs::write(&path, base).unwrap();
        let registry = caudal_core::Registry::new();
        let started = Supervisor::start(&crate::config::load(&path).unwrap(), registry.clone());
        let app = router(ReloadState {
            supervisor: started.supervisor,
            config_path: Some(path.clone()),
            edits: Default::default(),
        });
        let _live = registry.publish("cam1", caudal_core::BufferConfig::default()).unwrap();

        std::fs::write(
            &path,
            format!("{base}[[push.auto]]\nstreams = [\"cam*\"]\nurl = \"rtmp://127.0.0.1:9/live/$stream\"\n"),
        )
        .unwrap();
        let (st, body) = call(&app, "POST", "/api/v1/config/reload", None).await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert!(body["applied"].as_array().unwrap().iter().any(|a| a == "push.auto"), "{body}");

        let (_, list) = call(&app, "GET", "/api/v1/pushes", None).await;
        assert_eq!(
            list,
            serde_json::json!([{"stream": "cam*", "url": "rtmp://127.0.0.1:9/live/****", "section": "[[push.auto]]"}])
        );
        let (st, rules) = call(&app, "GET", "/api/v1/auto-pushes", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(rules[0]["kind"], "rtmp");
        assert_eq!(rules[0]["active"], serde_json::json!([{"stream": "cam1", "url": "rtmp://127.0.0.1:9/live/****"}]));
        assert_eq!((rules[0]["index"].clone(), rules[0]["enabled"].clone()), (0.into(), true.into()));
        assert!(rules[0]["idle"].is_null() && rules[0]["schedule"].is_null());

        // Runtime toggle: the push stops and the rule says why; the file
        // is not touched.
        let file = std::fs::read_to_string(&path).unwrap();
        let (st, rule) = call(&app, "POST", "/api/v1/auto-pushes/0/disable", None).await;
        assert_eq!(st, StatusCode::OK, "{rule}");
        assert_eq!(rule["idle"]["reason"], "disabled");
        assert_eq!(rule["active"], serde_json::json!([]));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), file);
        let (_, rule) = call(&app, "POST", "/api/v1/auto-pushes/0/enable", None).await;
        assert_eq!((rule["enabled"].clone(), rule["active"].as_array().unwrap().len()), (true.into(), 1));
        assert_eq!(call(&app, "POST", "/api/v1/auto-pushes/7/enable", None).await.0, StatusCode::NOT_FOUND);
        assert_eq!(call(&app, "POST", "/api/v1/auto-pushes/0/pause", None).await.0, StatusCode::NOT_FOUND);

        std::fs::write(&path, base).unwrap();
        let (st, _) = call(&app, "POST", "/api/v1/config/reload", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(call(&app, "GET", "/api/v1/auto-pushes", None).await.1, serde_json::json!([]));
    }

    #[tokio::test]
    async fn editing_without_a_config_file_is_a_conflict() {
        let started = Supervisor::start(&port0_config(), caudal_core::Registry::new());
        let app = router(ReloadState { supervisor: started.supervisor, config_path: None, edits: Default::default() });
        let (st, _) = call(&app, "POST", "/api/v1/pushes", Some(r#"{"stream":"s","url":"srt://h:1"}"#)).await;
        assert_eq!(st, StatusCode::CONFLICT);
    }

    /// Sections built once at startup must be reported, never dropped
    /// silently: `[admin]` and `[health]` arrived after the reload diff.
    #[tokio::test]
    async fn admin_and_health_changes_are_reported_as_requires_restart() {
        let registry = caudal_core::Registry::new();
        let started = Supervisor::start(&port0_config(), registry);

        let mut changed = port0_config();
        changed.health.no_keyframe_secs += 1;
        changed.admin = Some(Default::default());
        let report = started.supervisor.reload(changed).unwrap();
        assert!(report.requires_restart.contains(&"health".to_owned()), "{:?}", report.requires_restart);
        assert!(report.requires_restart.contains(&"admin".to_owned()), "{:?}", report.requires_restart);
    }
}
