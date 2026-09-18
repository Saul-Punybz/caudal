//! One accepted SRT connection: validates the stream id, claims a
//! `Publisher`, and turns MPEG-TS payloads into `caudal_core::Frame`s.
//!
//! Track announcement timing mirrors `caudal-rtmp`'s `session.rs`: tracks
//! are announced as soon as both video and audio init records are known,
//! or after a 2 s grace period with whatever showed up (an audio-only or
//! video-only stream is valid).

use std::net::SocketAddrV4;
use std::sync::{Arc, Weak};
use std::time::Duration;

use caudal_core::{BufferConfig, PublishError, Publisher, Registry, TrackInfo};
use parking_lot::Mutex;
use rsrt::SrtSocket;

use crate::demux::{DemuxEvent, Demuxer};
use crate::ts::TsDemux;

const TRACK_ANNOUNCE_DEADLINE: Duration = Duration::from_secs(2);
const TRACK_ANNOUNCE_POLL: Duration = Duration::from_millis(200);

#[derive(Default)]
struct Pending {
    video: Option<TrackInfo>,
    audio: Option<TrackInfo>,
    announced: bool,
}

/// State shared between the connection task and the announce-deadline timer.
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

/// Extracts the publish target from an SRT stream id: `publish/<name>` or
/// the SRT access-control form `#!::r=<name>,m=publish`. Any other `m=`
/// value, a missing name, or an id matching neither form is rejected
/// (`None`).
fn parse_publish_name(streamid: &str) -> Option<&str> {
    if let Some(name) = streamid.strip_prefix("publish/") {
        return (!name.is_empty()).then_some(name);
    }
    if let Some(rest) = streamid.strip_prefix("#!::") {
        let mut mode: Option<&str> = None;
        let mut name: Option<&str> = None;
        for field in rest.split(',') {
            let (key, value) = field.split_once('=')?;
            match key {
                "m" => mode = Some(value),
                "r" => name = Some(value),
                _ => {}
            }
        }
        if mode == Some("publish") {
            return name.filter(|n| !n.is_empty());
        }
        return None;
    }
    None
}

/// Drives one accepted SRT connection to completion. Never panics: a
/// malformed stream ends this connection only (via the ordinary recv-error
/// path), never the listener.
pub(crate) async fn handle(mut socket: SrtSocket, peer: SocketAddrV4, registry: Arc<Registry>, buffer: BufferConfig) {
    let streamid = socket.streamid().unwrap_or_default();
    let Some(name) = parse_publish_name(&streamid) else {
        tracing::debug!(%peer, %streamid, "srt rejected: not a publish stream id");
        return;
    };

    let publisher = match registry.publish(name, buffer) {
        Ok(p) => p,
        Err(PublishError::Busy(name)) => {
            tracing::warn!(%peer, %name, "srt rejected: stream already publishing");
            return;
        }
        Err(PublishError::InvalidName) => {
            tracing::warn!(%peer, name, "srt rejected: invalid stream name");
            return;
        }
    };
    tracing::info!(%peer, stream = %publisher.stream().name(), "srt publish started");

    let shared = Arc::new(Shared { publisher, pending: Mutex::new(Pending::default()) });
    spawn_announce_deadline(&shared);

    let mut ts = TsDemux::new();
    let mut demuxer = Demuxer::new();
    let mut units = Vec::new();
    let mut events = Vec::new();

    loop {
        match socket.recv().await {
            Ok(Some(payload)) => {
                ts.feed(&payload);
                units.clear();
                ts.drain(&mut units);
                for unit in units.drain(..) {
                    events.clear();
                    demuxer.consume(unit, &mut events);
                    for event in events.drain(..) {
                        match event {
                            DemuxEvent::VideoInit(info) => shared.update_video(info),
                            DemuxEvent::AudioInit(info) => shared.update_audio(info),
                            DemuxEvent::VideoFrame(frame) | DemuxEvent::AudioFrame(frame) => {
                                if let Err(err) = shared.publisher.push(frame) {
                                    tracing::trace!(%err, "dropped frame");
                                }
                            }
                        }
                    }
                }
            }
            Ok(None) => {
                tracing::debug!(%peer, "srt stream ended cleanly");
                break;
            }
            Err(err) => {
                tracing::debug!(%peer, %err, "srt connection ended");
                break;
            }
        }
    }
    // Dropping `shared` here drops its `Publisher`, ending the stream.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_form_accepted() {
        assert_eq!(parse_publish_name("publish/test"), Some("test"));
        assert_eq!(parse_publish_name("publish/"), None);
    }

    #[test]
    fn access_control_form_accepted() {
        assert_eq!(parse_publish_name("#!::r=test,m=publish"), Some("test"));
        assert_eq!(parse_publish_name("#!::m=publish,r=test"), Some("test"));
        assert_eq!(parse_publish_name("#!::u=alice,r=test,m=publish"), Some("test"));
    }

    #[test]
    fn play_and_other_modes_rejected() {
        assert_eq!(parse_publish_name("play/test"), None);
        assert_eq!(parse_publish_name("#!::r=test,m=request"), None);
        assert_eq!(parse_publish_name("#!::m=publish"), None);
        assert_eq!(parse_publish_name(""), None);
        assert_eq!(parse_publish_name("garbage"), None);
    }
}
