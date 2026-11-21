//! FLV messages (`VIDEODATA`, `AUDIODATA`, AMF0 data) of one published
//! stream into a [`caudal_core::Publisher`]: what an accepted RTMP publisher
//! ([`crate::session`]) and an `[[rtmp.pull]]` ([`crate::pull`]) both feed.
//! Tracks are announced once both sequence headers arrived, or after
//! [`TRACK_ANNOUNCE_DEADLINE`] with whichever did (audio-only and video-only
//! streams are valid).

use std::sync::{Arc, Weak};
use std::time::Duration;

use bytes::Bytes;
use caudal_core::{Frame, Publisher, TrackId, TrackInfo};
use parking_lot::Mutex;

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

/// One published stream's FLV demuxer. Dropping it drops the
/// [`Publisher`], which ends the stream.
pub struct FlvIngest {
    shared: Arc<Shared>,
}

impl FlvIngest {
    /// Must be called inside a tokio runtime (it spawns the announce
    /// deadline timer).
    pub fn new(publisher: Publisher) -> Self {
        let shared = Arc::new(Shared { publisher, pending: Mutex::new(Pending::default()), clock: Mutex::default() });
        spawn_announce_deadline(&shared);
        Self { shared }
    }

    /// The stream being published.
    pub fn publisher(&self) -> &Publisher {
        &self.shared.publisher
    }

    /// One `VIDEODATA` message body, with its RTMP timestamp (ms).
    pub fn video(&self, timestamp: u32, data: Bytes) {
        let shared = &self.shared;
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

    /// One `AUDIODATA` message body, with its RTMP timestamp (ms).
    pub fn audio(&self, timestamp: u32, data: Bytes) {
        let shared = &self.shared;
        match demux::demux_audio(data) {
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
        }
    }

    /// One AMF0 data message (`onCuePoint` SCTE-35 cues, `onMetaData`).
    pub fn amf0(&self, timestamp: u32, data: Bytes) {
        let shared = &self.shared;
        // The event id only has to be unique per stream; the message
        // timestamp is.
        if let Some(cue) = demux::parse_cue_point(shared.clock.lock().extend(timestamp), data.clone(), timestamp) {
            tracing::debug!(at_us = cue.at_us, kind = cue.kind.as_str(), "rtmp scte-35 cue in");
            if let Err(err) = shared.publisher.push_cue(cue) {
                tracing::trace!(%err, "dropped cue");
            }
        } else if let Some(fps) = demux::parse_metadata_fps(data) {
            shared.update_fps(fps);
        }
    }

    /// A framerate learned some other way than raw `onMetaData` bytes (an
    /// RTMP client library that parses metadata itself).
    pub fn set_fps(&self, fps: f64) {
        self.shared.update_fps(fps);
    }
}
