//! The output side shared by both engines: the clock shift that keeps
//! renditions on the source's timeline, and one lazily published rendition
//! stream.

use std::sync::Arc;

use caudal_core::{BufferConfig, Frame, Publisher, Registry, TrackId, TrackInfo, TrackKind};

/// Encoder input starts this far after zero, so a frame slightly older than
/// the first one (audio queued just before the join keyframe) never goes
/// negative on the 33-bit TS clock.
pub(crate) const HEADROOM_US: i64 = 10_000_000;

pub(crate) const VIDEO_OUT: TrackId = TrackId(0);
pub(crate) const AUDIO_OUT: TrackId = TrackId(1);

fn rescale(v: i64, from: i64, to: i64) -> i64 {
    (i128::from(v) * i128::from(to) / i128::from(from.max(1))) as i64
}

/// Maps source time onto the encoder's private clock and back. The origin is
/// the first source frame ever fed and never changes for the life of the
/// source, so an encoder restart continues on the same timeline.
#[derive(Default, Clone, Copy)]
pub(crate) struct Clock {
    origin_us: Option<i64>,
}

impl Clock {
    /// `frame` with its timestamps moved onto the encoder clock.
    pub fn onto_encoder(&mut self, info: &TrackInfo, frame: &Frame) -> Frame {
        let origin = *self.origin_us.get_or_insert_with(|| info.to_micros(frame.dts));
        let d = rescale(HEADROOM_US - origin, 1_000_000, i64::from(info.timescale));
        Frame { dts: frame.dts + d, pts: frame.pts + d, ..frame.clone() }
    }

    /// What to add to an encoder-clock timestamp on `timescale` to get
    /// back to the source clock.
    pub fn back_offset(&self, timescale: u32) -> i64 {
        rescale(self.origin_us.unwrap_or(0) - HEADROOM_US, 1_000_000, i64::from(timescale))
    }
}

/// One rendition stream `<source>+<label>`, published once its tracks are
/// known (so outputs never see a half-described stream), kept across
/// encoder restarts, ended when dropped.
pub(crate) struct RenditionOut {
    pub name: String,
    registry: Arc<Registry>,
    buffer: BufferConfig,
    publisher: Option<Publisher>,
    failed: bool,
    video: Option<TrackInfo>,
    audio: Option<TrackInfo>,
    expect_audio: bool,
    fps: Option<f64>,
    pending: Vec<Frame>,
    last_dts: [Option<i64>; 2],
    seen_key: bool,
}

/// Frames held while waiting for the audio track description before giving
/// up on it and publishing video alone.
const MAX_PENDING: usize = 300;

impl RenditionOut {
    pub fn new(registry: Arc<Registry>, name: String, buffer: BufferConfig, expect_audio: bool) -> Self {
        Self {
            name,
            registry,
            buffer,
            publisher: None,
            failed: false,
            video: None,
            audio: None,
            expect_audio,
            fps: None,
            pending: Vec::new(),
            last_dts: [None, None],
            seen_key: false,
        }
    }

    pub fn set_fps(&mut self, fps: f64) {
        self.fps = Some(fps);
    }

    /// Announces (or updates) a track. `info.id` must be `VIDEO_OUT` or
    /// `AUDIO_OUT`.
    pub fn set_track(&mut self, mut info: TrackInfo) {
        if info.kind() == TrackKind::Video {
            info.id = VIDEO_OUT;
            if let (Some(v), Some(fps)) = (info.video.as_mut(), self.fps) {
                v.fps.get_or_insert(fps);
            }
            self.video = Some(info);
        } else {
            info.id = AUDIO_OUT;
            self.audio = Some(info);
        }
        if let Some(p) = &self.publisher {
            let _ = p.set_tracks(self.tracks());
        }
    }

    pub fn audio_rate(&self) -> Option<u32> {
        self.audio.as_ref().map(|a| a.timescale)
    }

    fn tracks(&self) -> Vec<TrackInfo> {
        self.video.iter().chain(self.audio.iter()).cloned().collect()
    }

    pub fn push(&mut self, frame: Frame) {
        if self.failed {
            return;
        }
        let slot = (frame.track == AUDIO_OUT) as usize;
        if self.last_dts[slot].is_some_and(|l| frame.dts <= l) {
            tracing::debug!(stream = %self.name, dts = frame.dts, "non-monotonic rendition frame dropped");
            return;
        }
        if frame.track == VIDEO_OUT && !self.seen_key {
            if !frame.keyframe {
                return;
            }
            self.seen_key = true;
        }
        self.last_dts[slot] = Some(frame.dts);
        if let Some(p) = &self.publisher {
            let _ = p.push(frame);
            return;
        }
        self.pending.push(frame);
        let ready = self.video.is_some() && (self.audio.is_some() || !self.expect_audio);
        if !(ready || (self.video.is_some() && self.pending.len() >= MAX_PENDING)) {
            if self.pending.len() > MAX_PENDING * 4 {
                self.pending.clear();
            }
            return;
        }
        match self.registry.publish(&self.name, self.buffer) {
            Ok(p) => {
                let _ = p.set_tracks(self.tracks());
                for f in self.pending.drain(..) {
                    let _ = p.push(f);
                }
                self.publisher = Some(p);
            }
            Err(e) => {
                tracing::warn!(stream = %self.name, error = %e, "rendition not published");
                self.failed = true;
                self.pending.clear();
            }
        }
    }
}
