//! MPEG-TS in, a published stream out: whatever carries the TS (an SRT
//! connection Caudal accepted or called, a UDP socket), the bytes go to a
//! [`TsIngest`], which demuxes them and feeds a `Publisher`.
//!
//! Track announcement timing mirrors `caudal-rtmp`'s `session.rs`: tracks
//! are announced as soon as both video and audio init records are known,
//! or after a 2 s grace period with whatever showed up (an audio-only or
//! video-only stream is valid). Dropping the `TsIngest` drops the
//! publisher, ending the stream.

use std::sync::{Arc, Weak};
use std::time::Duration;

use caudal_core::{Publisher, TrackInfo};
use parking_lot::Mutex;

use crate::demux::{DemuxEvent, Demuxer};
use crate::ts::{EsUnit, TsDemux};

const TRACK_ANNOUNCE_DEADLINE: Duration = Duration::from_secs(2);
const TRACK_ANNOUNCE_POLL: Duration = Duration::from_millis(200);

#[derive(Default)]
struct Pending {
    video: Option<TrackInfo>,
    audio: Option<TrackInfo>,
    announced: bool,
}

/// State shared between the ingest and the announce-deadline timer.
struct Shared {
    publisher: Publisher,
    pending: Mutex<Pending>,
}

impl Shared {
    fn try_announce(&self, force: bool) {
        let mut pending = self.pending.lock();
        if pending.announced {
            return;
        }
        let (have_video, have_audio) = (pending.video.is_some(), pending.audio.is_some());
        if !have_video && !have_audio {
            return;
        }
        if !force && !(have_video && have_audio) {
            return;
        }
        let mut tracks = Vec::with_capacity(2);
        if let Some(v) = pending.video.clone() {
            tracks.push(v);
        }
        if let Some(a) = pending.audio.clone() {
            tracks.push(a);
        }
        pending.announced = true;
        drop(pending);
        if let Err(err) = self.publisher.set_tracks(tracks) {
            tracing::warn!(stream = %self.publisher.stream().name(), %err, "set_tracks failed");
        }
    }

    fn update_video(&self, info: TrackInfo) {
        self.pending.lock().video = Some(info);
        self.try_announce(false);
    }

    fn update_audio(&self, info: TrackInfo) {
        self.pending.lock().audio = Some(info);
        self.try_announce(false);
    }
}

fn spawn_announce_deadline(shared: &Arc<Shared>) {
    let weak: Weak<Shared> = Arc::downgrade(shared);
    tokio::spawn(async move {
        tokio::time::sleep(TRACK_ANNOUNCE_DEADLINE).await;
        loop {
            let Some(shared) = weak.upgrade() else { return };
            let (announced, have_any) = {
                let p = shared.pending.lock();
                (p.announced, p.video.is_some() || p.audio.is_some())
            };
            if announced {
                return;
            }
            if have_any {
                shared.try_announce(true);
                return;
            }
            drop(shared);
            tokio::time::sleep(TRACK_ANNOUNCE_POLL).await;
        }
    });
}

/// One live MPEG-TS input feeding one publisher. Must be created inside a
/// tokio runtime (it starts the track-announce timer).
pub struct TsIngest {
    shared: Arc<Shared>,
    ts: TsDemux,
    demuxer: Demuxer,
    units: Vec<EsUnit>,
    events: Vec<DemuxEvent>,
}

impl TsIngest {
    pub fn new(publisher: Publisher) -> Self {
        let shared = Arc::new(Shared { publisher, pending: Mutex::new(Pending::default()) });
        spawn_announce_deadline(&shared);
        Self { shared, ts: TsDemux::new(), demuxer: Demuxer::new(), units: Vec::new(), events: Vec::new() }
    }

    pub fn publisher(&self) -> &Publisher {
        &self.shared.publisher
    }

    /// Feeds TS bytes (any number of whole or partial 188-byte packets).
    pub fn feed(&mut self, payload: &[u8]) {
        self.ts.feed(payload);
        self.units.clear();
        self.ts.drain(&mut self.units);
        for unit in self.units.drain(..) {
            self.events.clear();
            self.demuxer.consume(unit, &mut self.events);
            for event in self.events.drain(..) {
                match event {
                    DemuxEvent::VideoInit(info) => self.shared.update_video(info),
                    DemuxEvent::AudioInit(info) => self.shared.update_audio(info),
                    DemuxEvent::VideoFrame(frame) | DemuxEvent::AudioFrame(frame) => {
                        if let Err(err) = self.shared.publisher.push(frame) {
                            tracing::trace!(%err, "dropped frame");
                        }
                    }
                    DemuxEvent::Cue(cue) => {
                        tracing::debug!(
                            stream = %self.shared.publisher.stream().name(),
                            at_us = cue.at_us,
                            kind = cue.kind.as_str(),
                            "scte-35 cue in"
                        );
                        if let Err(err) = self.shared.publisher.push_cue(cue) {
                            tracing::trace!(%err, "dropped cue");
                        }
                    }
                }
            }
        }
    }
}
