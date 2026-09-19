//! The per-connection RTMP session handler: bridges `scuffle_rtmp`'s
//! `SessionHandler` callbacks to a [`caudal_core::Publisher`] per published
//! stream.

use std::collections::HashMap;
use std::net::IpAddr;
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
    /// One clock for the whole stream: audio, video and data messages share
    /// RTMP's timestamp domain.
    clock: Mutex<demux::RtmpClock>,
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
            if let Some(fps) = pending.meta_fps
                && let Some(video) = info.video.as_mut()
            {
                video.fps = Some(fps);
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
        if let Some(tracks) = tracks_to_reannounce
            && let Err(err) = self.publisher.set_tracks(tracks)
        {
            tracing::warn!(stream = %self.publisher.stream().name(), %err, "set_tracks failed");
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
    /// The TCP peer's address, passed to every `authorize` call on this
    /// connection.
    peer_ip: IpAddr,
    streams: HashMap<u32, Arc<Shared>>,
}

impl Handler {
    pub(crate) fn new(registry: Arc<Registry>, app: String, buffer: BufferConfig, peer_ip: IpAddr) -> Self {
        Self { registry, app, buffer, peer_ip, streams: HashMap::new() }
    }
}

/// `ServerSessionError` (scuffle-rtmp 0.2.3) is a closed enum with no
/// "rejected"/"custom" variant we're allowed to add (see NOTES.md). Returning
/// *any* `Err` from `on_publish` still does what we need: `on_command_publish`
/// propagates it with `?` before sending `NetStream.Publish.Start`, straight
/// out of `ServerSession::run(self)`, which drops `self` — and with it the
/// `TcpStream` it owns — closing the connection immediately, no extra
/// round trip. `InvalidChunkSize(0)` is reused purely as an inert carrier to
/// trigger that unwind; its `Display` text is misleading and must never be
/// treated as the reason. The real reason is the `rtmp publish rejected` info
/// log emitted right before returning it.
fn reject_error() -> ServerSessionError {
    ServerSessionError::InvalidChunkSize(0)
}

impl SessionHandler for Handler {
    async fn on_publish(
        &mut self,
        stream_id: u32,
        app_name: &str,
        stream_name: &str,
    ) -> Result<(), ServerSessionError> {
        if app_name != self.app {
            tracing::info!(app = %app_name, stream = %stream_name, reason = "wrong_app", "rtmp publish rejected");
            // No Publisher is ever created for this stream_id; returning Err
            // ends the session and closes the TCP connection (see reject_error).
            return Err(reject_error());
        }

        // OBS and ffmpeg carry credentials in the stream key:
        // rtmp://host/live/<name>?token=<jwt>
        let (stream_name, query) = stream_name.split_once('?').unwrap_or((stream_name, ""));
        let token = query.split('&').find_map(|kv| kv.strip_prefix("token=")).filter(|t| !t.is_empty());
        if let Err(denied) =
            self.registry.authorize(caudal_core::Access::Publish, stream_name, token, Some(self.peer_ip)).await
        {
            tracing::info!(app = %app_name, stream = %stream_name, reason = ?denied, "rtmp publish rejected");
            return Err(reject_error());
        }

        match self.registry.publish(stream_name, self.buffer) {
            Ok(publisher) => {
                let shared =
                    Arc::new(Shared { publisher, pending: Mutex::new(Pending::default()), clock: Mutex::default() });
                spawn_announce_deadline(&shared);
                self.streams.insert(stream_id, shared);
                Ok(())
            }
            Err(PublishError::Busy(name)) => {
                tracing::info!(app = %app_name, stream = %name, reason = "busy", "rtmp publish rejected");
                Err(reject_error())
            }
            Err(PublishError::InvalidName) => {
                tracing::info!(app = %app_name, stream = %stream_name, reason = "invalid_name", "rtmp publish rejected");
                Err(reject_error())
            }
        }
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
            SessionData::Video { timestamp, data } => {
                match demux::demux_video(shared.clock.lock().extend(timestamp), data) {
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
                }
            }
            SessionData::Audio { timestamp, data } => match demux::demux_audio(data) {
                Some(AudioEvent::Init(info)) => shared.update_audio(info),
                Some(AudioEvent::Frame(raw)) => {
                    if let Some(sample_rate) = shared.audio_sample_rate() {
                        let ts = shared.clock.lock().extend(timestamp) * i64::from(sample_rate) / 1000;
                        let frame = Frame { track: TrackId(1), dts: ts, pts: ts, keyframe: true, data: raw };
                        if let Err(err) = shared.publisher.push(frame) {
                            tracing::trace!(%err, "dropped audio frame");
                        }
                    }
                }
                None => {}
            },
            SessionData::Amf0 { timestamp, data } => {
                // The event id only has to be unique per stream; the
                // message timestamp is.
                if let Some(cue) =
                    demux::parse_cue_point(shared.clock.lock().extend(timestamp), data.clone(), timestamp)
                {
                    tracing::debug!(at_us = cue.at_us, kind = cue.kind.as_str(), "rtmp scte-35 cue in");
                    if let Err(err) = shared.publisher.push_cue(cue) {
                        tracing::trace!(%err, "dropped cue");
                    }
                } else if let Some(fps) = demux::parse_metadata_fps(data) {
                    shared.update_fps(fps);
                }
            }
        }

        Ok(())
    }
}
