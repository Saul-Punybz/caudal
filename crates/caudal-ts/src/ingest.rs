//! MPEG-TS in, a published stream out: whatever carries the TS (an SRT
//! connection Caudal accepted or called, a UDP socket), the bytes go to a
//! [`TsIngest`], which demuxes them and feeds a `Publisher`.
//!
//! Track announcement timing mirrors `caudal-rtmp`'s `session.rs`: tracks
//! are announced as soon as both video and audio init records are known,
//! or after a 2 s grace period with whatever showed up (an audio-only or
//! video-only stream is valid). Dropping the `TsIngest` drops the
//! publisher, ending the stream.
//!
//! [`TsIngest::discontinuity`] marks a splice in the source (an HLS
//! `EXT-X-DISCONTINUITY`: another encoder, a restarted one): the frames
//! after it are moved onto the timeline where the previous ones stopped,
//! so the published stream stays monotonic whatever the new clock says.

use std::sync::{Arc, Weak};
use std::time::Duration;

use caudal_core::{Frame, Publisher, TrackId, TrackInfo};
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

/// Keeps the timeline continuous across [`TsIngest::discontinuity`]; also
/// used by the HLS pull of fMP4 segments. Register every track with
/// [`Rebase::timescale`], call [`Rebase::splice`] at a discontinuity and
/// pass every frame through [`Rebase::apply`].
#[derive(Default)]
pub struct Rebase {
    /// Added to every frame (and cue) time, microseconds.
    offset_us: i64,
    /// Set by a discontinuity: the next frame decides the new offset.
    pending: bool,
    /// Per track: timescale, last DTS pushed (µs, offset applied), and the
    /// step between its last two frames (µs).
    tracks: Vec<(TrackId, u32, i64, i64)>,
}

impl Rebase {
    /// Registers (or updates) a track's timescale.
    pub fn timescale(&mut self, info: &TrackInfo) {
        match self.tracks.iter_mut().find(|t| t.0 == info.id) {
            Some(t) => t.1 = info.timescale,
            None => self.tracks.push((info.id, info.timescale, i64::MIN, 0)),
        }
    }

    /// Where the previous frames stopped: the end of the latest one.
    fn continue_at(&self) -> Option<i64> {
        self.tracks.iter().filter(|t| t.2 != i64::MIN).map(|t| t.2 + t.3.max(0)).max()
    }

    /// The source is spliced: the next frame continues where the latest
    /// frame so far ended, whatever its own timestamp says.
    pub fn splice(&mut self) {
        self.pending = true;
    }

    /// The offset added to frame times, microseconds (for cues).
    pub fn offset_us(&self) -> i64 {
        self.offset_us
    }

    /// Shifts `frame` onto the continuous timeline.
    pub fn apply(&mut self, frame: &mut Frame) {
        let Some(idx) = self.tracks.iter().position(|t| t.0 == frame.track) else { return };
        let scale = i128::from(self.tracks[idx].1.max(1));
        let to_us = |ticks: i64| (i128::from(ticks) * 1_000_000 / scale) as i64;
        if self.pending {
            self.pending = false;
            if let Some(at) = self.continue_at() {
                self.offset_us = at - to_us(frame.dts);
            }
        }
        let shift = (i128::from(self.offset_us) * scale / 1_000_000) as i64;
        frame.dts += shift;
        frame.pts += shift;
        let t = &mut self.tracks[idx];
        let now = to_us(frame.dts);
        if t.2 != i64::MIN && now > t.2 {
            t.3 = now - t.2;
        }
        t.2 = now;
    }
}

/// One live MPEG-TS input feeding one publisher. Must be created inside a
/// tokio runtime (it starts the track-announce timer).
pub struct TsIngest {
    shared: Arc<Shared>,
    ts: TsDemux,
    demuxer: Demuxer,
    units: Vec<EsUnit>,
    events: Vec<DemuxEvent>,
    rebase: Rebase,
}

impl TsIngest {
    pub fn new(publisher: Publisher) -> Self {
        let shared = Arc::new(Shared { publisher, pending: Mutex::new(Pending::default()) });
        spawn_announce_deadline(&shared);
        Self {
            shared,
            ts: TsDemux::new(),
            demuxer: Demuxer::new(),
            units: Vec::new(),
            events: Vec::new(),
            rebase: Rebase::default(),
        }
    }

    pub fn publisher(&self) -> &Publisher {
        &self.shared.publisher
    }

    /// Feeds TS bytes (any number of whole or partial 188-byte packets).
    pub fn feed(&mut self, payload: &[u8]) {
        self.ts.feed(payload);
        self.units.clear();
        self.ts.drain(&mut self.units);
        self.consume_units();
    }

    /// The source is spliced here: hands out the access units still being
    /// assembled (they belong to the old side), then shifts whatever comes
    /// next to continue where the last frame ended. Codec parameters that
    /// change across the splice are not re-announced.
    pub fn discontinuity(&mut self) {
        self.units.clear();
        self.ts.drain(&mut self.units);
        self.ts.flush(&mut self.units);
        self.consume_units();
        self.rebase.pending = true;
    }

    fn consume_units(&mut self) {
        let mut units = std::mem::take(&mut self.units);
        for unit in units.drain(..) {
            self.events.clear();
            self.demuxer.consume(unit, &mut self.events);
            for event in self.events.drain(..) {
                match event {
                    DemuxEvent::VideoInit(info) => {
                        self.rebase.timescale(&info);
                        self.shared.update_video(info);
                    }
                    DemuxEvent::AudioInit(info) => {
                        self.rebase.timescale(&info);
                        self.shared.update_audio(info);
                    }
                    DemuxEvent::VideoFrame(mut frame) | DemuxEvent::AudioFrame(mut frame) => {
                        self.rebase.apply(&mut frame);
                        if let Err(err) = self.shared.publisher.push(frame) {
                            tracing::trace!(%err, "dropped frame");
                        }
                    }
                    DemuxEvent::Cue(mut cue) => {
                        cue.at_us += self.rebase.offset_us;
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
        self.units = units;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use caudal_core::Codec;

    fn info(id: u32, codec: Codec, timescale: u32) -> TrackInfo {
        TrackInfo { id: TrackId(id), codec, timescale, init: Bytes::new(), lang: None, video: None, audio: None }
    }

    fn frame(track: u32, dts: i64) -> Frame {
        Frame { track: TrackId(track), dts, pts: dts + 3000, keyframe: true, data: Bytes::new() }
    }

    #[test]
    fn a_discontinuity_continues_the_timeline() {
        let mut r = Rebase::default();
        r.timescale(&info(0, Codec::H264, 90_000));
        r.timescale(&info(1, Codec::Aac, 48_000));
        // Old side: video at 25 fps up to 10 s, audio 1024-sample frames.
        for i in 0..=250 {
            let mut f = frame(0, i * 3600);
            r.apply(&mut f);
            assert_eq!(f.dts, i * 3600, "untouched before any discontinuity");
        }
        for i in 0..=468 {
            r.apply(&mut frame(1, i * 1024));
        }
        // New side: an encoder restarted at t = 0.
        r.pending = true;
        let mut v = frame(0, 0);
        r.apply(&mut v);
        // The latest frame ended at 10 s + 40 ms (video) vs 468*1024/48k + 21.3 ms
        // = 9.984 + 0.021 s (audio): video wins.
        assert_eq!(v.dts, 10_040_000 * 90 / 1000);
        assert_eq!(v.pts - v.dts, 3000, "composition offset kept");
        let mut a = frame(1, 0);
        r.apply(&mut a);
        assert_eq!(a.dts, 10_040_000 * 48 / 1000, "both tracks shift by the same time");
        let mut later = frame(0, 3600);
        r.apply(&mut later);
        assert_eq!(later.dts, v.dts + 3600, "the offset stays");
    }
}
