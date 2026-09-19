//! Per-output counters, shared between the send task, `GET
//! /api/v1/multicast` and `/metrics`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::Serialize;

use crate::MulticastTarget;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum State {
    /// The source stream is not live.
    Waiting,
    /// Sending.
    Live,
    /// The socket could not be opened (no such interface, no route to the
    /// group); retried while the stream stays live.
    Error,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            State::Waiting => "waiting",
            State::Live => "live",
            State::Error => "error",
        }
    }
}

pub(crate) struct OutputStatus {
    stream: String,
    group: String,
    format: &'static str,
    state: Mutex<(State, Instant)>,
    last_error: Mutex<Option<String>>,
    packets: AtomicU64,
    bytes: AtomicU64,
    send_errors: AtomicU64,
    /// Microseconds the last datagram left after its media deadline.
    lag_us: AtomicU64,
    max_lag_us: AtomicU64,
}

/// One row of `GET /api/v1/multicast`.
#[derive(Serialize)]
pub(crate) struct OutputJson {
    pub stream: String,
    pub group: String,
    pub format: &'static str,
    pub state: &'static str,
    pub since_secs: u64,
    /// Datagrams sent (each 7 TS packets, plus an RTP header for `rtp`).
    pub packets_sent: u64,
    /// UDP payload bytes sent.
    pub bytes_sent: u64,
    pub send_errors: u64,
    /// How late the last datagram left relative to its frame's place on
    /// the stream's clock: the smoothing working through a keyframe.
    pub pacing_lag_ms: f64,
    pub max_pacing_lag_ms: f64,
    pub last_error: Option<String>,
}

impl OutputStatus {
    pub(crate) fn new(target: &MulticastTarget) -> Self {
        Self {
            stream: target.stream.clone(),
            group: target.group.to_string(),
            format: target.format.as_str(),
            state: Mutex::new((State::Waiting, Instant::now())),
            last_error: Mutex::new(None),
            packets: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            send_errors: AtomicU64::new(0),
            lag_us: AtomicU64::new(0),
            max_lag_us: AtomicU64::new(0),
        }
    }

    fn set(&self, state: State) {
        *self.state.lock() = (state, Instant::now());
    }

    pub(crate) fn set_waiting(&self) {
        self.set(State::Waiting);
        self.lag_us.store(0, Ordering::Relaxed);
    }

    pub(crate) fn set_live(&self) {
        self.set(State::Live);
        *self.last_error.lock() = None;
    }

    pub(crate) fn set_error(&self, err: String) {
        *self.last_error.lock() = Some(err);
        self.set(State::Error);
    }

    pub(crate) fn sent(&self, bytes: usize, lag: Duration) {
        self.packets.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
        let lag = lag.as_micros() as u64;
        self.lag_us.store(lag, Ordering::Relaxed);
        self.max_lag_us.fetch_max(lag, Ordering::Relaxed);
    }

    /// A failed `send_to` (e.g. the interface went down): the datagram is
    /// lost, sending continues. Returns whether this is the first failure
    /// of this output, so the caller warns once and logs the rest at debug.
    pub(crate) fn send_failed(&self, err: &std::io::Error) -> bool {
        let first = self.send_errors.fetch_add(1, Ordering::Relaxed) == 0;
        *self.last_error.lock() = Some(err.to_string());
        first
    }

    pub(crate) fn to_json(&self) -> OutputJson {
        let (state, since) = *self.state.lock();
        OutputJson {
            stream: self.stream.clone(),
            group: self.group.clone(),
            format: self.format,
            state: state.as_str(),
            since_secs: since.elapsed().as_secs(),
            packets_sent: self.packets.load(Ordering::Relaxed),
            bytes_sent: self.bytes.load(Ordering::Relaxed),
            send_errors: self.send_errors.load(Ordering::Relaxed),
            pacing_lag_ms: self.lag_us.load(Ordering::Relaxed) as f64 / 1000.0,
            max_pacing_lag_ms: self.max_lag_us.load(Ordering::Relaxed) as f64 / 1000.0,
            last_error: self.last_error.lock().clone(),
        }
    }
}
