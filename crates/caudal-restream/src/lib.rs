//! Multistreaming: pushes each configured [`RestreamTarget`] out over RTMP
//! or RTMPS (YouTube, Twitch, Facebook, or any RTMP ingest, including
//! `crates/caudal-rtmp`'s own). Entry points fixed by the orchestrator.
//!
//! Shaped after `crates/caudal-srt/src/push.rs`, which does the same job
//! for SRT out: one task per target, waiting for the source stream to
//! publish (and picking up a stream that is already live), reconnecting
//! with backoff while the source stays live, stopping cleanly when it
//! ends. See `crate::push` for the state machine and `crate::client` for
//! the RTMP/RTMPS transport (`rml_rtmp` plus our own tokio/rustls I/O; see
//! `REUSE.md` for why).
//!
//! Every push subscribes with `Stream::subscribe_internal`, so it is never
//! counted as a viewer, and starts at the live edge, which
//! `caudal_core::Stream::subscribe` always resolves to the newest
//! keyframe — a restream target never gets a frame it can't decode first.
//!
//! The stream key never appears in a log line, an error message, or the
//! status API: every place that could print the configured URL prints
//! [`url::redact_url`]'s output instead. See `crate::url` and
//! `crate::status`.

mod client;
mod flv;
mod http;
mod push;
mod status;
mod url;

use std::sync::Arc;

use caudal_core::Registry;
use status::TargetStatus;

/// One or more streams to push to a remote RTMP/RTMPS ingest.
#[derive(Debug, Clone)]
pub struct RestreamConfig {
    pub targets: Vec<RestreamTarget>,
}

/// Push `stream` to `url` (`rtmp://` or `rtmps://host[:port]/app/key`)
/// while it is live, reconnecting with backoff on failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestreamTarget {
    pub stream: String,
    pub url: String,
}

/// A handle to the running restream tasks; also the `axum` state for
/// [`router`]. Cheap to clone (an `Arc` of the per-target status list).
#[derive(Clone)]
pub struct RestreamHandle {
    targets: Arc<Vec<Arc<TargetStatus>>>,
}

/// Starts pushing every configured target (current and future publishes of
/// its source stream) and returns immediately. Must be called from inside a
/// tokio runtime: each target runs as its own spawned task, so a stuck or
/// slow target never affects any other.
pub fn start(registry: Arc<Registry>, cfg: RestreamConfig) -> RestreamHandle {
    let mut targets = Vec::with_capacity(cfg.targets.len());
    for target in cfg.targets {
        let status = Arc::new(TargetStatus::new(target.stream.clone(), url::redact_url(&target.url)));
        let registry = registry.clone();
        let task_status = status.clone();
        tokio::spawn(async move {
            push::run(target, registry, task_status).await;
        });
        targets.push(status);
    }
    RestreamHandle { targets: Arc::new(targets) }
}

/// `GET /api/v1/restreams` → one row per configured target: `{ stream,
/// target (redacted), state, bytes_sent, since_secs, last_error }`.
pub fn router(handle: RestreamHandle) -> axum::Router {
    http::router(handle)
}
