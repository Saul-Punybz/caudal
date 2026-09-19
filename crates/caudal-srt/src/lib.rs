//! SRT ingest: accepts SRT callers carrying MPEG-TS and feeds a
//! [`caudal_core::Publisher`]. Entry point fixed by the orchestrator.
//!
//! Built on `rsrt` (pure-Rust SRT, live mode, verified against libsrt
//! 1.5.6) for the transport and `mpeg2ts` for TS/PES parsing; see
//! `NOTES.md` for the demux reuse decision and the AVCC/hvcC/ADTS
//! conversion rules. One task per accepted connection: a malformed stream
//! or a rejected caller only ever affects its own connection.
//!
//! [`serve`] (the listener) and [`start_pushes`] (caller-mode `[[srt.push]]`
//! targets) run as two independent subsystems so a config reload can
//! restart either without the other: e.g. a changed `[[srt.push]]` list is
//! applied with [`PushHandle::reload`], never touching the listener or any
//! already-accepted connection.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use caudal_core::{BufferConfig, Registry};
use parking_lot::Mutex;
use rsrt::{SrtListener, SrtOptions};

mod connection;
use caudal_ts::{demux, mux, ts};
mod play;
mod push;

#[derive(Debug, Clone)]
pub struct SrtConfig {
    pub bind: SocketAddr,
    /// Receiver latency (TSBPD), milliseconds.
    pub latency_ms: u32,
    /// When set, callers must use this passphrase (AES).
    pub passphrase: Option<String>,
    pub buffer: BufferConfig,
}

/// Push `stream` to `url` (`srt://host:port?streamid=...&passphrase=...`),
/// reconnecting while the stream is live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SrtPush {
    pub stream: String,
    pub url: String,
}

struct PushEntry {
    push: SrtPush,
    task: tokio::task::JoinHandle<()>,
}

/// A handle to the running `[[srt.push]]` tasks. Cheap to clone.
#[derive(Clone)]
pub struct PushHandle {
    entries: Arc<Mutex<Vec<PushEntry>>>,
}

fn spawn_push(registry: Arc<Registry>, push: SrtPush) -> PushEntry {
    let task_push = push.clone();
    let task = tokio::spawn(async move {
        push::run(task_push, registry).await;
    });
    PushEntry { push, task }
}

/// Starts every configured push (current and future publishes of its
/// source stream) and returns immediately. Must be called inside a tokio
/// runtime: each push is its own task, so a stuck or slow target never
/// affects any other, the listener, or any accepted connection.
pub fn start_pushes(registry: Arc<Registry>, pushes: Vec<SrtPush>) -> PushHandle {
    let entries = pushes.into_iter().map(|p| spawn_push(registry.clone(), p)).collect();
    PushHandle { entries: Arc::new(Mutex::new(entries)) }
}

impl PushHandle {
    /// Applies a new push list: a push whose `stream` and `url` are both
    /// unchanged keeps its task (and in-flight connection) untouched;
    /// removed pushes are stopped, changed or new ones (re)started.
    pub fn reload(&self, registry: &Arc<Registry>, pushes: Vec<SrtPush>) {
        let mut entries = self.entries.lock();
        let mut remaining = std::mem::take(&mut *entries);
        let mut next = Vec::with_capacity(pushes.len());
        for p in pushes {
            if let Some(pos) = remaining.iter().position(|e| e.push == p) {
                next.push(remaining.remove(pos));
            } else {
                next.push(spawn_push(registry.clone(), p));
            }
        }
        for gone in remaining {
            gone.task.abort();
        }
        *entries = next;
    }
}

/// Listens until the future is dropped or the socket fails. A caller's
/// stream id selects the stream and direction: `publish/<name>` or
/// `#!::r=<name>,m=publish` to send into Caudal; `play/<name>` or
/// `#!::r=<name>,m=request` to receive a stream as MPEG-TS (batch 6).
/// `[[srt.push]]` targets run separately: see [`start_pushes`].
pub async fn serve(cfg: SrtConfig, registry: Arc<Registry>) -> std::io::Result<()> {
    // A live publisher sends media continuously, so 3 s without any means it
    // is gone. A caller killed outright never sends SRT's shutdown, and the
    // default 5 s peer-idle timer (keepalives count) ends the stream late.
    let mut opts = SrtOptions::default()
        .latency(Duration::from_millis(u64::from(cfg.latency_ms)))
        .data_idle_timeout(Duration::from_secs(3));
    if let Some(passphrase) = cfg.passphrase.clone() {
        opts = opts.passphrase(passphrase);
    }

    // Encryption is enforced by `rsrt` at the handshake itself (both-or-
    // neither passphrase, and it must be the right one): a mismatched
    // caller is rejected before `accept()` ever sees it, so no extra
    // passphrase check is needed here.
    let mut listener = SrtListener::bind(cfg.bind, opts)
        .await
        .map_err(|err| std::io::Error::other(format!("srt bind failed: {err}")))?;
    tracing::info!(bind = %cfg.bind, "srt listening");

    loop {
        // One accept error must not stop the listener for good while
        // /healthz keeps saying OK. Same policy as axum::serve.
        let (socket, peer) = match listener.accept().await {
            Ok(conn) => conn,
            Err(err) => {
                tracing::warn!(error = %err, "srt accept failed; retrying");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };
        let registry = registry.clone();
        let buffer = cfg.buffer;
        // Each connection runs on its own task: a panic, a slow client or a
        // malformed stream in one never affects any other publisher.
        tokio::spawn(async move {
            connection::handle(socket, peer, registry, buffer).await;
        });
    }
}

#[cfg(test)]
mod reload_tests {
    use super::*;

    fn push(stream: &str, url: &str) -> SrtPush {
        SrtPush { stream: stream.into(), url: url.into() }
    }

    fn task_ids(handle: &PushHandle) -> Vec<tokio::task::Id> {
        handle.entries.lock().iter().map(|e| e.task.id()).collect()
    }

    #[tokio::test]
    async fn unchanged_push_keeps_its_task() {
        let registry = Registry::new();
        let handle = start_pushes(registry.clone(), vec![push("a", "srt://x:1?streamid=publish/a")]);
        let before = task_ids(&handle);
        handle.reload(&registry, vec![push("a", "srt://x:1?streamid=publish/a")]);
        assert_eq!(task_ids(&handle), before, "unchanged push must not be restarted");
    }

    #[tokio::test]
    async fn changed_removed_and_added_pushes() {
        let registry = Registry::new();
        let handle = start_pushes(
            registry.clone(),
            vec![push("a", "srt://x:1?streamid=publish/a"), push("b", "srt://x:1?streamid=publish/b")],
        );
        let before = task_ids(&handle);

        handle.reload(
            &registry,
            vec![push("a", "srt://x:1?streamid=publish/CHANGED"), push("c", "srt://x:1?streamid=publish/c")],
        );

        let after = handle.entries.lock();
        assert_eq!(after.len(), 2);
        assert!(after.iter().any(|e| e.push.stream == "c"));
        assert!(!after.iter().any(|e| e.push.stream == "b"), "removed push is gone");
        let a_task = after.iter().find(|e| e.push.stream == "a").unwrap().task.id();
        assert!(!before.contains(&a_task), "changed push got a new task");
    }
}
