//! RTMP play: one viewer reading a stream through the listener
//! (`rtmp://host/app/stream`, the way VLC, ffplay, OBS's media source and
//! another server's RTMP pull consume it).
//!
//! [`Play`] turns a [`Subscriber`]'s events into the messages
//! `scuffle_rtmp`'s session (vendored, see `vendor/README.md`) writes to the
//! socket: `onMetaData` and the sequence headers after every track change,
//! then the FLV tag of each frame from the newest keyframe on (via
//! [`crate::flv`], the same tags `caudal-restream` pushes). One video track
//! and one audio track are carried, the first of each kind RTMP can encode.
//!
//! Memory stays bounded for a slow viewer: the session writes and flushes
//! one message before asking for the next, so nothing queues here beyond
//! a track change's two or three headers. A viewer that falls behind falls
//! off the stream's own ring and is moved to the next keyframe
//! ([`Event::Lagged`]); one that stops reading altogether is dropped when a
//! write blocks too long (`PLAY_WRITE_TIMEOUT` in the vendored session).

use std::collections::VecDeque;

use caudal_core::{Event, Session, Subscriber, TrackId, TrackInfo, TrackKind};
use scuffle_rtmp::session::server::PlayMessage;

use crate::flv;

/// One viewer's play state, owned by the connection's handler.
pub(crate) struct Play {
    sub: Subscriber,
    session: Session,
    stream: String,
    video: Option<TrackInfo>,
    audio: Option<TrackInfo>,
    /// Tracks whose sequence header has gone out since the last track
    /// change. Sent just before the track's first frame, at its timestamp.
    headers_sent: Vec<TrackId>,
    /// Until a video keyframe went out, video frames are skipped (the
    /// player cannot decode them). Set again after a lag.
    need_keyframe: bool,
    queue: VecDeque<PlayMessage>,
}

/// What [`Play::next`] wants the handler to do.
pub(crate) enum Next {
    Send(PlayMessage),
    /// The operator kicked this viewer: close without a goodbye.
    Kicked,
}

/// RTMP milliseconds for a track timestamp. Wrapping to `u32` matches
/// RTMP's own 32-bit timestamp wrap (players extend it again).
fn to_rtmp_ms(info: &TrackInfo, ts: i64) -> u32 {
    (info.to_micros(ts) / 1000) as u32
}

fn message_len(msg: &PlayMessage) -> usize {
    match msg {
        PlayMessage::Video { data, .. } | PlayMessage::Audio { data, .. } | PlayMessage::Amf0 { data, .. } => {
            data.len()
        }
        _ => 0,
    }
}

impl Play {
    pub(crate) fn new(sub: Subscriber, session: Session, stream: String) -> Self {
        Self {
            sub,
            session,
            stream,
            video: None,
            audio: None,
            headers_sent: Vec::new(),
            need_keyframe: true,
            queue: VecDeque::new(),
        }
    }

    pub(crate) fn stream(&self) -> &str {
        &self.stream
    }

    /// The next message for the viewer. Cancel-safe: whatever an event
    /// produced is queued before this returns, and the waits are the
    /// subscriber's and the session's own cancel-safe futures.
    pub(crate) async fn next(&mut self) -> Next {
        loop {
            if let Some(msg) = self.queue.pop_front() {
                self.session.add_bytes(message_len(&msg));
                return Next::Send(msg);
            }
            let ev = tokio::select! {
                ev = self.sub.recv() => ev,
                () = self.session.kicked() => return Next::Kicked,
            };
            self.handle(ev);
        }
    }

    fn handle(&mut self, ev: Event) {
        match ev {
            Event::TracksChanged => self.tracks_changed(),
            Event::Frame(frame) => {
                let info = match (&self.video, &self.audio) {
                    (Some(v), _) if v.id == frame.track => v.clone(),
                    (_, Some(a)) if a.id == frame.track => a.clone(),
                    _ => return,
                };
                let video = info.kind() == TrackKind::Video;
                if video && self.need_keyframe {
                    if !frame.keyframe {
                        return;
                    }
                    self.need_keyframe = false;
                }
                let timestamp = to_rtmp_ms(&info, frame.dts);
                if !self.headers_sent.contains(&info.id) {
                    self.headers_sent.push(info.id);
                    if let Some(data) = flv::sequence_header(&info) {
                        self.queue.push_back(media(video, timestamp, data));
                    }
                }
                let cts_ms = ((info.to_micros(frame.pts) - info.to_micros(frame.dts)) / 1000) as i32;
                if let Some(data) = flv::frame_tag(&info, &frame, cts_ms) {
                    self.queue.push_back(media(video, timestamp, data));
                }
            }
            Event::Lagged { skipped } => {
                // The subscriber already resumed at a join point (a video
                // keyframe); only make sure no stray inter frame precedes it.
                tracing::debug!(stream = %self.stream, skipped, "rtmp play lagged; resuming at the next keyframe");
                self.need_keyframe = true;
            }
            // RTMP viewers get no cues: onCuePoint out is not implemented.
            Event::Cue(_) => {}
            Event::End => self.queue.push_back(PlayMessage::End),
        }
    }

    fn tracks_changed(&mut self) {
        let tracks = self.sub.tracks();
        let pick = |kind: TrackKind| tracks.iter().find(|t| t.kind() == kind && flv::supported(t.codec)).cloned();
        let video = pick(TrackKind::Video);
        let audio = pick(TrackKind::Audio);
        for t in &tracks {
            let carried = [&video, &audio].iter().any(|c| c.as_ref().is_some_and(|c| c.id == t.id));
            if !carried && matches!(t.kind(), TrackKind::Video | TrackKind::Audio) {
                tracing::debug!(stream = %self.stream, codec = t.codec.as_str(), "rtmp play: track not carried");
            }
        }
        let key = |t: &Option<TrackInfo>| t.as_ref().map(|t| (t.id, t.init.clone()));
        if key(&video) != key(&self.video) {
            self.need_keyframe = true;
        }
        self.video = video;
        self.audio = audio;
        self.headers_sent.clear();
        self.queue.push_back(PlayMessage::Amf0 {
            timestamp: 0,
            data: flv::metadata(self.video.as_ref(), self.audio.as_ref()),
        });
    }
}

fn media(video: bool, timestamp: u32, data: bytes::Bytes) -> PlayMessage {
    if video { PlayMessage::Video { timestamp, data } } else { PlayMessage::Audio { timestamp, data } }
}
