//! TEMPORARY (feat/m12-wiring only): the shape of the output API that
//! `crates/caudal` is wired against, so that branch compiles and its
//! config/API/metrics tests run before `output.rs` lands. Nothing here sends
//! a frame. (The pull half is the real `ingest.rs`.)
//!
//! DELETE AT MERGE: this file, the two `wiring-stubs` lines in `lib.rs`, the
//! `wiring-stubs` feature in this crate's `Cargo.toml` and the
//! `features = ["wiring-stubs"]` on `caudal-omt` in `crates/caudal/Cargo.toml`.
//! The real modules must export the same names (or `crates/caudal/src/omt.rs`
//! is adjusted to theirs; it is the only user).

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use caudal_core::Registry;
use open_media_transport::command::Quality;
use open_media_transport::discovery::Discovery;

/// One `[[omt.output]]`: send `stream` as the OMT source `name`.
#[derive(Clone)]
pub struct OutputConfig {
    pub stream: String,
    /// Source name; announced as `MACHINE (name)`.
    pub name: String,
    pub quality: Quality,
    /// VMX encoder threads; 0 = pick from the frame size (libomtnet's rule).
    pub encoder_threads: usize,
    /// Shared announcer; `None` when discovery could not start (receivers
    /// can still connect by `omt://host:port`).
    pub discovery: Option<Arc<Discovery>>,
}

/// Live counters of one output.
#[derive(Debug, Default)]
pub struct OutputStats {
    /// Video frames handed to the OMT sender.
    pub frames_sent: AtomicU64,
    /// Frames dropped because the encoder fell behind.
    pub dropped_queue_full: AtomicU64,
    /// Frames that failed to decode (H.264 or audio) before VMX encoding.
    pub dropped_decode: AtomicU64,
    /// Video receivers connected now.
    pub receivers: AtomicUsize,
    /// Combined tally of every receiver.
    pub preview: AtomicBool,
    pub program: AtomicBool,
}

impl OutputStats {
    pub fn dropped(&self) -> Vec<(&'static str, u64)> {
        vec![
            ("queue_full", self.dropped_queue_full.load(Ordering::Relaxed)),
            ("decode_error", self.dropped_decode.load(Ordering::Relaxed)),
        ]
    }
}

/// A running output as the API and `/metrics` see it.
#[derive(Clone)]
pub struct OutputStatus {
    pub stream: String,
    pub name: String,
    /// `MACHINE (name)` once announced.
    pub full_name: Option<String>,
    /// `omt://MACHINE:port` once the sender is listening.
    pub url: Option<String>,
    pub stats: Arc<OutputStats>,
}

/// Handle to the running `[[omt.output]]`s. Cheap to clone.
#[derive(Clone)]
pub struct OutputHandle {
    entries: Arc<Mutex<Vec<OutputStatus>>>,
}

fn output_status(o: OutputConfig) -> OutputStatus {
    OutputStatus { stream: o.stream, name: o.name, full_name: None, url: None, stats: Arc::default() }
}

pub fn start_outputs(_registry: Arc<Registry>, outputs: Vec<OutputConfig>) -> OutputHandle {
    OutputHandle { entries: Arc::new(Mutex::new(outputs.into_iter().map(output_status).collect())) }
}

impl OutputHandle {
    pub fn reload(&self, outputs: Vec<OutputConfig>) {
        let mut e = self.entries.lock().unwrap();
        let old = std::mem::take(&mut *e);
        *e = outputs
            .into_iter()
            .map(|p| match old.iter().find(|o| o.stream == p.stream && o.name == p.name) {
                Some(o) => o.clone(),
                None => output_status(p),
            })
            .collect();
    }

    pub fn status(&self) -> Vec<OutputStatus> {
        self.entries.lock().unwrap().clone()
    }

    pub fn stop(&self) {
        self.entries.lock().unwrap().clear();
    }
}
