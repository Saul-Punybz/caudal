//! Frames in, one continuous fragmented MP4 out: the init segment once,
//! then `moof` + `mdat` fragments. Boxes come from `caudal_cmaf::fmp4`
//! (the writer LL-HLS and recording use); this file only decides what
//! goes into each fragment and on which clock.
//!
//! - The *primary* track (video, or audio when there is no video) decides
//!   the cuts: a fragment closes at every primary keyframe (one per GOP),
//!   or after [`MAX_FRAGMENT_MS`] inside a long GOP so memory stays small.
//!   An audio-only stream is cut every [`AUDIO_FRAGMENT_MS`].
//! - The output starts at the first primary keyframe. Every track's decode
//!   time is counted from that instant (shared, so audio and video stay in
//!   sync), which makes the first `tfdt` 0 (or the audio's small lead).
//! - A sample's duration is only known when the next one on its track
//!   arrives, so each track holds back one sample.
//! - The secondary track (audio next to video) goes into the fragment that
//!   is open when its sample starts.
//! - A timestamp jump (backwards, or forward by more than [`JUMP_SECS`])
//!   re-bases that track so its timeline continues where it was: decode
//!   times only ever grow.
//!
//! Synchronous, so tests drive it directly.

use bytes::Bytes;
use caudal_cmaf::fmp4::{self, Mp4Track, Run, Sample};
use caudal_core::{Codec, Frame, TrackId, TrackInfo, TrackKind};

/// The longest fragment inside a long GOP.
pub const MAX_FRAGMENT_MS: i64 = 2000;
/// Fragment length for an audio-only stream.
pub const AUDIO_FRAGMENT_MS: i64 = 1000;
/// A gap bigger than this between two frames of a track is a timestamp
/// jump, not a frame duration.
pub const JUMP_SECS: i64 = 5;

/// Whether this output can write `t`'s codec into an MP4 track.
pub fn carriable(t: &TrackInfo) -> bool {
    match t.codec {
        Codec::H264 | Codec::H265 => !t.init.is_empty(),
        Codec::Aac => t.init.len() >= 2,
        Codec::Opus => caudal_cmaf::opus::parse_opus_head(&t.init).is_some(),
        _ => false,
    }
}

struct Lane {
    mp4_id: u32,
    core_id: TrackId,
    timescale: i64,
    /// Every sync sample (audio).
    all_key: bool,
    /// Source dts that maps to output dts 0; set on the first sample.
    offset: Option<i64>,
    pending: Option<Sample>,
    /// Output dts right after the last committed sample.
    next_dts: Option<i64>,
    last_dur: u32,
    samples: Vec<Sample>,
}

impl Lane {
    fn new(t: &Mp4Track) -> Self {
        let ts = t.info.timescale.max(1);
        let audio = t.info.kind() == TrackKind::Audio;
        let default_dur = if audio { 1024 } else { ts / 30 };
        Self {
            mp4_id: t.track_id,
            core_id: t.info.id,
            timescale: i64::from(ts),
            all_key: audio,
            offset: None,
            pending: None,
            next_dts: None,
            last_dur: default_dur.max(1),
            samples: Vec::new(),
        }
    }

    fn micros(&self, ticks: i64) -> i64 {
        (i128::from(ticks) * 1_000_000 / i128::from(self.timescale)) as i64
    }

    fn ticks(&self, micros: i64) -> i64 {
        (i128::from(micros) * i128::from(self.timescale) / 1_000_000) as i64
    }

    /// The sample for `f` on the output clock, or `None` if it falls
    /// before the start. Closes the pending sample (its duration is now
    /// known) into `samples`.
    fn take(&mut self, f: &Frame, start_us: i64) -> Option<Sample> {
        let offset = *self.offset.get_or_insert_with(|| {
            // Shared start instant, on this track's clock.
            (i128::from(start_us) * i128::from(self.timescale) / 1_000_000) as i64
        });
        let mut dts = f.dts - offset;
        if self.pending.is_none() && self.next_dts.is_none() && dts < 0 {
            // Audio from just before the first video keyframe.
            self.offset = None;
            return None;
        }
        let expect = self.pending.as_ref().map(|p| p.dts).or(self.next_dts);
        if let Some(prev) = expect {
            let floor = match &self.pending {
                Some(p) => p.dts + 1,
                None => prev,
            };
            if dts < floor || dts - prev > JUMP_SECS * self.timescale {
                // Continue where the track was, one duration later.
                let at = match &self.pending {
                    Some(p) => p.dts + i64::from(self.last_dur),
                    None => prev,
                };
                self.offset = Some(f.dts - at);
                dts = at;
            }
        }
        if let Some(mut p) = self.pending.take() {
            let d = (dts - p.dts).clamp(1, i64::from(u32::MAX)) as u32;
            p.dur = d;
            self.last_dur = d;
            self.next_dts = Some(p.dts + i64::from(d));
            self.samples.push(p);
        }
        Some(Sample {
            dts,
            cts: (f.pts - f.dts).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
            dur: 0,
            key: f.keyframe || self.all_key,
            data: f.data.clone(),
        })
    }

    /// Commits the held-back sample with the last known duration.
    fn flush_pending(&mut self) {
        if let Some(mut p) = self.pending.take() {
            p.dur = self.last_dur;
            self.next_dts = Some(p.dts + i64::from(p.dur));
            self.samples.push(p);
        }
    }

    fn span_us(&self) -> i64 {
        match (self.samples.first(), self.samples.last()) {
            (Some(a), Some(b)) => self.micros(b.dts + i64::from(b.dur) - a.dts),
            _ => 0,
        }
    }
}

/// One viewer's muxer.
pub struct Mux {
    init: Bytes,
    tracks: Vec<Mp4Track>,
    primary: Lane,
    secondary: Option<Lane>,
    /// Source micros of the first primary keyframe.
    start_us: Option<i64>,
    /// Drop primary frames until a keyframe (start, and after a lag).
    need_key: bool,
    sequence: u32,
    out: Vec<Bytes>,
}

impl Mux {
    /// A muxer for these tracks (the first one is primary: video when
    /// there is video), or `None` if the init segment cannot be written.
    pub fn new(tracks: Vec<Mp4Track>) -> Option<Self> {
        let init = fmp4::init_segment(&tracks)?;
        let mut lanes = tracks.iter().map(Lane::new);
        let primary = lanes.next()?;
        let secondary = lanes.next();
        Some(Self { init, tracks, primary, secondary, start_us: None, need_key: true, sequence: 0, out: Vec::new() })
    }

    /// `ftyp` + `moov`.
    pub fn init(&self) -> &Bytes {
        &self.init
    }

    /// The tracks written into the init segment.
    pub fn tracks(&self) -> &[Mp4Track] {
        &self.tracks
    }

    /// Fragments completed since the last call.
    pub fn take_output(&mut self) -> Vec<Bytes> {
        std::mem::take(&mut self.out)
    }

    pub fn push(&mut self, f: &Frame) {
        if f.track == self.primary.core_id {
            self.push_primary(f);
        } else if self.secondary.as_ref().is_some_and(|l| l.core_id == f.track) {
            self.push_secondary(f);
        }
    }

    fn push_primary(&mut self, f: &Frame) {
        let key = f.keyframe || self.primary.all_key;
        if self.need_key {
            if !key {
                return;
            }
            self.need_key = false;
        }
        let first_us = self.primary.micros(f.dts);
        let start_us = *self.start_us.get_or_insert(first_us);
        let had_pending = self.primary.pending.is_some();
        let Some(s) = self.primary.take(f, start_us) else { return };
        if had_pending {
            let span = self.primary.span_us();
            let limit = if self.primary.all_key { AUDIO_FRAGMENT_MS } else { MAX_FRAGMENT_MS } * 1000;
            let cut = if self.primary.all_key { span >= limit } else { key || span >= limit };
            if cut {
                let boundary = self.primary.micros(s.dts);
                self.close_fragment(Some(boundary));
            }
        }
        self.primary.pending = Some(s);
    }

    fn push_secondary(&mut self, f: &Frame) {
        let Some(start_us) = self.start_us else { return };
        let lane = self.secondary.as_mut().expect("secondary lane");
        let Some(s) = lane.take(f, start_us) else { return };
        lane.pending = Some(s);
        // Primary stalled (video stopped, audio goes on): do not hold
        // audio without bound.
        if lane.span_us() > 2 * MAX_FRAGMENT_MS * 1000 {
            self.close_fragment(None);
        }
    }

    /// Writes the open fragment: every committed primary sample, and the
    /// secondary samples that start before `boundary` (output micros; all
    /// of them when `None`).
    fn close_fragment(&mut self, boundary: Option<i64>) {
        let primary = std::mem::take(&mut self.primary.samples);
        let secondary = match self.secondary.as_mut() {
            Some(l) => {
                let n = match boundary {
                    Some(b) => {
                        let lim = l.ticks(b);
                        l.samples.iter().take_while(|s| s.dts < lim).count()
                    }
                    None => l.samples.len(),
                };
                l.samples.drain(..n).collect()
            }
            None => Vec::new(),
        };
        if primary.is_empty() && secondary.is_empty() {
            return;
        }
        self.sequence += 1;
        let mut runs = vec![Run { track_id: self.primary.mp4_id, samples: &primary }];
        if let Some(l) = &self.secondary {
            runs.push(Run { track_id: l.mp4_id, samples: &secondary });
        }
        self.out.push(fmp4::fragment(self.sequence, &runs));
    }

    /// Writes everything held back: at the end of the stream, or before a
    /// gap (the viewer lagged and the next frames start at a later
    /// keyframe).
    pub fn flush(&mut self) {
        self.primary.flush_pending();
        if let Some(l) = self.secondary.as_mut() {
            l.flush_pending();
        }
        self.close_fragment(None);
    }

    /// The viewer was moved forward: write what is held and wait for the
    /// next keyframe.
    pub fn lagged(&mut self) {
        self.flush();
        self.need_key = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use caudal_core::{AudioParams, VideoParams};

    fn video() -> TrackInfo {
        TrackInfo {
            id: TrackId(0),
            codec: Codec::H264,
            timescale: 90_000,
            // avcC: version 1, High 4.0, one SPS and one PPS (dummy bytes).
            init: Bytes::from_static(&[
                0x01, 0x64, 0x00, 0x28, 0xFF, 0xE1, 0x00, 0x04, 0x67, 0x64, 0x00, 0x28, 0x01, 0x00, 0x02, 0x68, 0xEE,
            ]),
            lang: None,
            video: Some(VideoParams { width: 640, height: 360, fps: None }),
            audio: None,
        }
    }

    fn audio() -> TrackInfo {
        TrackInfo {
            id: TrackId(1),
            codec: Codec::Aac,
            timescale: 48_000,
            init: Bytes::from_static(&[0x11, 0x90]),
            lang: None,
            video: None,
            audio: Some(AudioParams { sample_rate: 48_000, channels: 2 }),
        }
    }

    fn frame(track: u32, dts: i64, key: bool) -> Frame {
        Frame { track: TrackId(track), dts, pts: dts, keyframe: key, data: Bytes::from_static(&[0, 0, 0, 1, 0x65]) }
    }

    fn mux() -> Mux {
        Mux::new(vec![Mp4Track { track_id: 1, info: video() }, Mp4Track { track_id: 2, info: audio() }]).unwrap()
    }

    #[test]
    fn one_fragment_per_gop_starting_at_a_keyframe() {
        let mut m = mux();
        // Starts mid-GOP: dropped until the keyframe at 30.
        let base = 1_000_000i64;
        for i in 0..90i64 {
            m.push(&frame(0, base + i * 3000, i % 30 == 0 && i > 0));
            m.push(&frame(1, (base + i * 3000) * 48_000 / 90_000, true));
        }
        let out = m.take_output();
        // Keyframes at 30 and 60: one GOP closed (30..60).
        assert_eq!(out.len(), 1);
        m.flush();
        assert_eq!(m.take_output().len(), 1);
    }

    #[test]
    fn timestamps_only_grow_across_a_jump() {
        let mut m = Mux::new(vec![Mp4Track { track_id: 1, info: video() }]).unwrap();
        m.push(&frame(0, 0, true));
        m.push(&frame(0, 3000, false));
        // Backwards jump: re-based to continue.
        m.push(&frame(0, 100, true));
        assert_eq!(m.primary.samples.iter().map(|s| s.dts).collect::<Vec<_>>(), Vec::<i64>::new());
        assert_eq!(m.primary.pending.as_ref().unwrap().dts, 6000);
        m.push(&frame(0, 3100, false));
        assert_eq!(m.primary.pending.as_ref().unwrap().dts, 9000);
    }

    #[test]
    fn carriable_codecs() {
        assert!(carriable(&video()));
        assert!(carriable(&audio()));
        let mut m2 = video();
        m2.codec = Codec::Mpeg2Video;
        assert!(!carriable(&m2));
    }
}
