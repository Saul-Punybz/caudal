//! Per-target status, shared between the push task and the HTTP API.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use parking_lot::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StateKind {
    /// The source stream is not live; waiting for it to be published.
    Waiting,
    /// The source stream is live; dialing the target.
    Connecting,
    /// Connected and pushing frames.
    Live,
    /// A connection attempt or an established push failed; backing off
    /// before trying again, while the source stream is still live.
    Retrying,
}

impl StateKind {
    fn as_str(self) -> &'static str {
        match self {
            StateKind::Waiting => "waiting",
            StateKind::Connecting => "connecting",
            StateKind::Live => "live",
            StateKind::Retrying => "retrying",
        }
    }
}

pub(crate) struct TargetStatus {
    stream: String,
    /// `scheme://host/app/****`: the key never lives here.
    target_redacted: String,
    state: Mutex<StateKind>,
    since: Mutex<Instant>,
    bytes_sent: AtomicU64,
    last_error: Mutex<Option<String>>,
}

impl TargetStatus {
    pub(crate) fn new(stream: String, target_redacted: String) -> Self {
        Self {
            stream,
            target_redacted,
            state: Mutex::new(StateKind::Waiting),
            since: Mutex::new(Instant::now()),
            bytes_sent: AtomicU64::new(0),
            last_error: Mutex::new(None),
        }
    }

    fn set_state(&self, s: StateKind) {
        *self.state.lock() = s;
        *self.since.lock() = Instant::now();
    }

    pub(crate) fn set_waiting(&self) {
        self.set_state(StateKind::Waiting);
    }

    pub(crate) fn set_connecting(&self) {
        self.set_state(StateKind::Connecting);
    }

    pub(crate) fn set_live(&self) {
        self.set_state(StateKind::Live);
        *self.last_error.lock() = None;
    }

    /// A connect or push attempt failed; `err` is never the raw target URL
    /// (callers pass messages built from parse/IO/protocol errors, which
    /// never contain the stream key).
    pub(crate) fn set_retrying(&self, err: impl Into<String>) {
        *self.last_error.lock() = Some(err.into());
        self.set_state(StateKind::Retrying);
    }

    pub(crate) fn add_bytes(&self, n: u64) {
        self.bytes_sent.fetch_add(n, Ordering::Relaxed);
    }

    pub(crate) fn to_json(&self) -> super::http::RestreamStatusJson {
        super::http::RestreamStatusJson {
            stream: self.stream.clone(),
            target: self.target_redacted.clone(),
            state: self.state.lock().as_str(),
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            since_secs: self.since.lock().elapsed().as_secs(),
            last_error: self.last_error.lock().clone(),
        }
    }
}
