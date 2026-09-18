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
use parking_lot::Mutex;
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

struct Entry {
    target: RestreamTarget,
    status: Arc<TargetStatus>,
    task: tokio::task::JoinHandle<()>,
}

/// A handle to the running restream tasks; also the `axum` state for
/// [`router`]. Cheap to clone (an `Arc` of the target list).
#[derive(Clone)]
pub struct RestreamHandle {
    entries: Arc<Mutex<Vec<Entry>>>,
}

fn spawn_target(registry: Arc<Registry>, target: RestreamTarget) -> Entry {
    let status = Arc::new(TargetStatus::new(target.stream.clone(), url::redact_url(&target.url)));
    let task_status = status.clone();
    let task_target = target.clone();
    let task = tokio::spawn(async move {
        push::run(task_target, registry, task_status).await;
    });
    Entry { target, status, task }
}

/// Starts pushing every configured target (current and future publishes of
/// its source stream) and returns immediately. Must be called from inside a
/// tokio runtime: each target runs as its own spawned task, so a stuck or
/// slow target never affects any other.
pub fn start(registry: Arc<Registry>, cfg: RestreamConfig) -> RestreamHandle {
    let entries = cfg.targets.into_iter().map(|t| spawn_target(registry.clone(), t)).collect();
    RestreamHandle { entries: Arc::new(Mutex::new(entries)) }
}

impl RestreamHandle {
    /// One status row per currently configured target, in configuration
    /// order (for `GET /api/v1/restreams`).
    pub(crate) fn statuses(&self) -> Vec<http::RestreamStatusJson> {
        self.entries.lock().iter().map(|e| e.status.to_json()).collect()
    }

    /// Applies a new target list: unchanged targets (same stream + url) are
    /// left running untouched, so their status (bytes sent, connection
    /// state) survives a reload; removed targets are stopped, changed or
    /// new ones (re)spawned. A target's own reconnect never affects any
    /// other target or any ingest/output subsystem.
    pub fn reload(&self, registry: &Arc<Registry>, targets: Vec<RestreamTarget>) {
        let mut entries = self.entries.lock();
        let old = std::mem::take(&mut *entries);
        let mut remaining: Vec<Entry> = old;
        let mut next = Vec::with_capacity(targets.len());
        for target in targets {
            if let Some(pos) = remaining.iter().position(|e| e.target == target) {
                next.push(remaining.remove(pos));
            } else {
                next.push(spawn_target(registry.clone(), target));
            }
        }
        for gone in remaining {
            gone.task.abort();
        }
        *entries = next;
    }
}

/// `GET /api/v1/restreams` → one row per configured target: `{ stream,
/// target (redacted), state, bytes_sent, since_secs, last_error }`.
pub fn router(handle: RestreamHandle) -> axum::Router {
    http::router(handle)
}

#[cfg(test)]
mod reload_tests {
    use super::*;

    fn target(stream: &str, url: &str) -> RestreamTarget {
        RestreamTarget { stream: stream.into(), url: url.into() }
    }

    fn task_ids(handle: &RestreamHandle) -> Vec<tokio::task::Id> {
        handle.entries.lock().iter().map(|e| e.task.id()).collect()
    }

    #[tokio::test]
    async fn unchanged_targets_keep_their_task_and_status() {
        let registry = Registry::new();
        let handle = start(registry.clone(), RestreamConfig { targets: vec![target("a", "rtmp://x/y/z")] });
        let before = task_ids(&handle);
        // Same list again: nothing should be restarted.
        handle.reload(&registry, vec![target("a", "rtmp://x/y/z")]);
        assert_eq!(task_ids(&handle), before, "unchanged target must not be restarted");
        assert_eq!(handle.statuses().len(), 1);
    }

    #[tokio::test]
    async fn changed_and_removed_and_added_targets() {
        let registry = Registry::new();
        let handle = start(
            registry.clone(),
            RestreamConfig { targets: vec![target("a", "rtmp://x/y/1"), target("b", "rtmp://x/y/2")] },
        );
        let before = task_ids(&handle);

        // `a`'s url changes (restarted), `b` is removed (stopped), `c` is
        // new (started).
        handle.reload(&registry, vec![target("a", "rtmp://x/y/CHANGED"), target("c", "rtmp://x/y/3")]);

        let after = handle.entries.lock();
        assert_eq!(after.len(), 2);
        assert!(after.iter().any(|e| e.target.stream == "a" && e.target.url == "rtmp://x/y/CHANGED"));
        assert!(after.iter().any(|e| e.target.stream == "c"));
        assert!(!after.iter().any(|e| e.target.stream == "b"), "removed target is gone");
        let a_task = after.iter().find(|e| e.target.stream == "a").unwrap().task.id();
        assert!(!before.contains(&a_task), "changed target got a new task");
    }
}
