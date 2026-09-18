//! The per-connection RTMP session handler: bridges `scuffle_rtmp`'s
//! `SessionHandler` callbacks to a [`caudal_core::Publisher`] per published
//! stream.

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use caudal_core::{BufferConfig, Frame, PublishError, Publisher, Registry, TrackId, TrackInfo};
use parking_lot::Mutex;
use scuffle_rtmp::session::server::{ServerSessionError, SessionData, SessionHandler};

use crate::demux::{self, AudioEvent, VideoEvent};

/// How long we wait for both a video and an audio sequence header before
/// announcing tracks with whatever showed up (an audio-only or video-only
/// stream is valid).
const TRACK_ANNOUNCE_DEADLINE: Duration = Duration::from_secs(2);
const TRACK_ANNOUNCE_POLL: Duration = Duration::from_millis(200);

#[derive(Default)]
struct Pending {
    video: Option<TrackInfo>,
    audio: Option<TrackInfo>,
    meta_fps: Option<f64>,
    announced: bool,
}

/// State shared between the connection task and the deadline timer task for
/// one published stream.
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
        let have_video = pending.video.is_some();
        let have_audio = pending.audio.is_some();
        if !have_video && !have_audio {
            return;
        }
        if !force && !(have_video && have_audio) {
            return;
        }

        let mut tracks = Vec::with_capacity(2);
        if let Some(video) = pending.video.clone() {
            tracks.push(video);
        }
        if let Some(audio) = pending.audio.clone() {
            tracks.push(audio);
        }
        pending.announced = true;
        drop(pending);

        if let Err(err) = self.publisher.set_tracks(tracks) {
            tracing::warn!(stream = %self.publisher.stream().name(), %err, "set_tracks failed");
        }
    }

    fn update_video(&self, mut info: TrackInfo) {
        {
            let mut pending = self.pending.lock();
            if let Some(fps) = pending.meta_fps {
                if let Some(video) = info.video.as_mut() {
                    video.fps = Some(fps);
                }
            }
            pending.video = Some(info);
        }
        self.try_announce(false);
    }

    fn update_audio(&self, info: TrackInfo) {
        {
            self.pending.lock().audio = Some(info);
        }
        self.try_announce(false);
    }

    /// Applies a framerate learned from `onMetaData`. Cheap and rare enough
    /// that re-announcing tracks that were already announced is fine.
    fn update_fps(&self, fps: f64) {
        let tracks_to_reannounce = {
            let mut pending = self.pending.lock();
            pending.meta_fps = Some(fps);
            if let Some(video) = pending.video.as_mut().and_then(|t| t.video.as_mut()) {
                video.fps = Some(fps);
            }
            pending.announced.then(|| {
                let mut tracks = Vec::with_capacity(2);
                if let Some(video) = pending.video.clone() {
                    tracks.push(video);
                }
                if let Some(audio) = pending.audio.clone() {
                    tracks.push(audio);
                }
                tracks
            })
        };
        if let Some(tracks) = tracks_to_reannounce {
            if let Err(err) = self.publisher.set_tracks(tracks) {
                tracing::warn!(stream = %self.publisher.stream().name(), %err, "set_tracks failed");
            }
        }
    }

    fn audio_sample_rate(&self) -> Option<u32> {
        self.pending.lock().audio.as_ref().and_then(|t| t.audio).map(|a| a.sample_rate)
    }
}

fn spawn_announce_deadline(shared: &Arc<Shared>) {
    let weak: Weak<Shared> = Arc::downgrade(shared);
    tokio::spawn(async move {
        tokio::time::sleep(TRACK_ANNOUNCE_DEADLINE).await;
        loop {
            let Some(shared) = weak.upgrade() else { return };
            let (announced, have_any) = {
                let pending = shared.pending.lock();
                (pending.announced, pending.video.is_some() || pending.audio.is_some())
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

/// One RTMP connection. A connection may publish more than one stream (each
/// with its own RTMP stream id), though real encoders publish exactly one.
pub(crate) struct Handler {
    registry: Arc<Registry>,
    app: String,
    buffer: BufferConfig,
    streams: HashMap<u32, Arc<Shared>>,
}

impl Handler {
    pub(crate) fn new(registry: Arc<Registry>, app: String, buffer: BufferConfig) -> Self {
        Self { registry, app, buffer, streams: HashMap::new() }
    }
}

impl SessionHandler for Handler {
    async fn on_publish(&mut self, stream_id: u32, app_name: &str, stream_name: &str) -> Result<(), ServerSessionError> {
        if app_name != self.app {
            tracing::warn!(app = %app_name, expected = %self.app, stream = %stream_name, "rejecting publish: wrong app");
            // Accept the RTMP handshake but never create a Publisher: no data
            // for this stream_id will ever be published to the registry.
            return Ok(());
        }

        match self.registry.publish(stream_name, self.buffer) {
            Ok(publisher) => {
                let shared = Arc::new(Shared { publisher, pending: Mutex::new(Pending::default()) });
                spawn_announce_deadline(&shared);
                self.streams.insert(stream_id, shared);
            }
            Err(PublishError::Busy(name)) => {
                tracing::warn!(stream = %name, "rejecting publish: already publishing");
            }
            Err(PublishError::InvalidName) => {
                tracing::warn!(stream = %stream_name, "rejecting publish: invalid stream name");
            }
        }

        Ok(())
    }

    async fn on_unpublish(&mut self, stream_id: u32) -> Result<(), ServerSessionError> {
        // Dropping the Shared (and its Publisher) ends the stream immediately.
        self.streams.remove(&stream_id);
        Ok(())
    }

    async fn on_data(&mut self, stream_id: u32, data: SessionData) -> Result<(), ServerSessionError> {
        let Some(shared) = self.streams.get(&stream_id) else {
            // Data for a stream we rejected or never announced: drop it.
            return Ok(());
        };

        match data {
            SessionData::Video { timestamp, data } => match demux::demux_video(timestamp, data) {
                Some(VideoEvent::Init(info)) => shared.update_video(info),
                Some(VideoEvent::Frame(frame)) => {
                    // Frames that arrive before tracks are announced are
                    // rejected by the buffer as UnknownTrack; that is the
                    // intended drop behavior, not an error worth logging loudly.
                    if let Err(err) = shared.publisher.push(frame) {
                        tracing::trace!(%err, "dropped video frame");
                    }
                }
                None => {}
            },
            SessionData::Audio { timestamp, data } => match demux::demux_audio(data) {
                Some(AudioEvent::Init(info)) => shared.update_audio(info),
                Some(AudioEvent::Frame(raw)) => {
                    if let Some(sample_rate) = shared.audio_sample_rate() {
                        let ts = i64::from(timestamp) * i64::from(sample_rate) / 1000;
                        let frame = Frame { track: TrackId(1), dts: ts, pts: ts, keyframe: true, data: raw };
                        if let Err(err) = shared.publisher.push(frame) {
                            tracing::trace!(%err, "dropped audio frame");
                        }
                    }
                }
                None => {}
            },
            SessionData::Amf0 { data, .. } => {
                if let Some(fps) = demux::parse_metadata_fps(data) {
                    shared.update_fps(fps);
                }
            }
        }

        Ok(())
    }
}
