//! Hot config reload: `POST /api/v1/config/reload` and, on Unix, SIGHUP.
//! Both re-read the TOML file the process was started with, validate it
//! (an invalid file is rejected and the running config keeps going — see
//! `config::load`), and hand it to [`crate::subsystems::Supervisor::reload`]
//! to apply the smallest correct change per section.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::Serialize;

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

/// `POST /api/v1/config/reload` → `200 { applied, restarted,
/// requires_restart }`, or `400 { error }` when the file is invalid or
/// missing.
pub fn router(state: ReloadState) -> Router {
    Router::new().route("/api/v1/config/reload", post(reload_config)).with_state(state)
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
        let state = ReloadState { supervisor: started.supervisor, config_path: None };
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
        let state = ReloadState { supervisor: started.supervisor, config_path: Some(path) };
        let err = reload_from_disk(&state).unwrap_err();
        assert!(err.contains("http_bnid"), "{err}");
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
