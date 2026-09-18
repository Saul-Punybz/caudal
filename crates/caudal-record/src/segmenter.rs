//! Frames in, CMAF segments out, for one recording.
//!
//! The timeline rules are the LL-HLS packager's (`caudal-hls/src/packager.rs`),
//! minus the playlist window and the part bookkeeping:
//! - The *primary* track (video, or audio in an audio-only stream) decides
//!   every cut. A segment starts on a primary keyframe once the current one
//!   has run for `segment_ms`; it runs long if the GOP is longer.
//! - Inside a segment, frames are written as fragments (`moof` + `mdat`) of
//!   about `CHUNK_MS`, so a long GOP never sits in memory whole: each
//!   fragment is appended to the segment's temp file as soon as it closes.
//! - A frame's duration is only known when the next frame on its track
//!   arrives, so each track holds back one frame.
//! - The secondary track (audio next to video) goes into whichever fragment
//!   is open when its sample *starts* before the fragment's end time.
//!
//! Synchronous and clock-free (the caller passes the wall clock), so tests
//! drive it directly.

use std::collections::VecDeque;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use caudal_cmaf::fmp4::{self, Mp4Track, Run, Sample};
use caudal_core::{Codec, Frame, TrackId, TrackInfo, TrackKind};

/// Target length of one fragment inside a segment.
const CHUNK_MS: i64 = 1000;
/// Added to every decode time so small negative timestamps stay valid.
const SHIFT_SECS: i64 = 10;
/// A gap bigger than this between consecutive frames of one track is a
/// timestamp jump, not a frame duration.
const JUMP_SECS: i64 = 5;
/// After a lag, a gap bigger than this is marked as a discontinuity.
const GAP_MICROS: i64 = 500_000;

/// What the segmenter produced, in order.
#[derive(Debug)]
pub(crate) enum Out {
    /// A segment starts; its fragments follow.
    Open {
        discontinuity: bool,
        /// Wall clock of the segment's first frame.
        pdt: SystemTime,
    },
    Fragment(Bytes),
    /// The open segment is complete.
    Close {
        /// Seconds, on the primary track's clock.
        duration: f64,
    },
}

struct Lane {
    track_id: u32,
    timescale: u32,
    core_id: TrackId,
    codec: Codec,
    shift: i64,
    pending: Option<Sample>,
    last_dur: u32,
}

impl Lane {
    fn new(track_id: u32, info: &TrackInfo) -> Self {
        let default_dur = match info.kind() {
            TrackKind::Audio => 1024,
            _ => info.timescale / 30,
        };
        Self {
            track_id,
            timescale: info.timescale.max(1),
            core_id: info.id,
            codec: info.codec,
            shift: SHIFT_SECS * i64::from(info.timescale.max(1)),
            pending: None,
            last_dur: default_dur.max(1),
        }
    }

    fn fallback_dur(&self, data: &Bytes) -> u32 {
        if self.codec == Codec::Opus { caudal_cmaf::opus::frame_duration_samples(data) } else { self.last_dur }
    }

    fn sample(&self, f: &Frame, audio: bool) -> Sample {
        Sample {
            dts: f.dts + self.shift,
            cts: (f.pts - f.dts).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
            dur: 0,
            key: f.keyframe || audio,
            data: f.data.clone(),
        }
    }

    fn micros(&self, ticks: i64) -> i64 {
        (i128::from(ticks) * 1_000_000 / i128::from(self.timescale)) as i64
    }
}

pub(crate) struct Segmenter {
    segment_ms: i64,
    primary: Option<Lane>,
    secondary: Option<Lane>,
    primary_is_audio: bool,
    /// `ftyp` + `moov` for the current tracks.
    pub init: Option<Bytes>,
    /// The tracks written into `init`.
    pub tracks: Vec<Mp4Track>,

    chunk: Vec<Sample>,
    queue: VecDeque<Sample>,
    /// Primary dts at which the open segment began.
    seg_start: Option<i64>,
    /// Primary ticks written into the open segment so far.
    seg_ticks: i64,
    /// `Open` not emitted yet: it goes out with the segment's first fragment,
    /// so a segment that never gets a fragment leaves no trace.
    pending_open: Option<Out>,
    seg_emitted: bool,
    floor: Option<i64>,
    waiting_key: bool,
    resume_from: Option<i64>,
    discontinuity_next: bool,
    any_output: bool,
    fragment_seq: u32,
    /// Wall clock and primary micros of the last fresh start: segment
    /// program-date-times follow the media clock from there.
    anchor: Option<(SystemTime, i64)>,
    out: Vec<Out>,
}

impl Segmenter {
    pub fn new(segment_ms: u32) -> Self {
        Self {
            segment_ms: i64::from(segment_ms.max(100)),
            primary: None,
            secondary: None,
            primary_is_audio: false,
            init: None,
            tracks: Vec::new(),
            chunk: Vec::new(),
            queue: VecDeque::new(),
            seg_start: None,
            seg_ticks: 0,
            pending_open: None,
            seg_emitted: false,
            floor: None,
            waiting_key: true,
            resume_from: None,
            discontinuity_next: false,
            any_output: false,
            fragment_seq: 0,
            anchor: None,
            out: Vec::new(),
        }
    }

    /// Everything produced since the last call.
    pub fn take(&mut self) -> Vec<Out> {
        std::mem::take(&mut self.out)
    }

    /// The track list changed. Returns true if the output changed (a new
    /// init segment): what was open is flushed first, so the caller can
    /// close its recording after `take()` and start a new one.
    pub fn set_tracks(&mut self, tracks: &[TrackInfo]) -> bool {
        let video = tracks.iter().find(|t| matches!(t.codec, Codec::H264 | Codec::H265) && !t.init.is_empty());
        let audio = tracks.iter().find(|t| match t.codec {
            Codec::Aac => t.init.len() >= 2,
            Codec::Opus => caudal_cmaf::opus::parse_opus_head(&t.init).is_some(),
            _ => false,
        });
        let mut mp4 = Vec::new();
        if let Some(v) = video {
            mp4.push(Mp4Track { track_id: 1, info: v.clone() });
        }
        if let Some(a) = audio {
            mp4.push(Mp4Track { track_id: mp4.len() as u32 + 1, info: a.clone() });
        }
        let mut init = fmp4::init_segment(&mp4);
        if init.is_none() && mp4.len() == 2 {
            mp4.retain(|t| fmp4::init_segment(std::slice::from_ref(t)).is_some());
            for (i, t) in mp4.iter_mut().enumerate() {
                t.track_id = i as u32 + 1;
            }
            init = fmp4::init_segment(&mp4);
        }
        let unchanged = init == self.init
            && self.primary.as_ref().map(|l| l.core_id) == mp4.first().map(|t| t.info.id)
            && self.secondary.as_ref().map(|l| l.core_id) == mp4.get(1).map(|t| t.info.id);
        if unchanged {
            return false;
        }
        self.flush();
        self.discontinuity_next = false;
        self.any_output = false;
        self.resume_from = None;
        self.anchor = None;
        self.queue.clear();
        self.floor = None;
        self.waiting_key = true;
        self.init = init;
        let mut lanes = mp4.iter().map(|t| Lane::new(t.track_id, &t.info));
        self.primary = lanes.next();
        self.secondary = lanes.next();
        self.primary_is_audio = mp4.first().is_some_and(|t| t.info.kind() == TrackKind::Audio);
        if self.init.is_none() {
            self.primary = None;
            self.secondary = None;
            mp4.clear();
        }
        self.tracks = mp4;
        true
    }

    /// Feeds one frame. `now` is the wall clock at which it was received.
    pub fn push(&mut self, f: &Frame, now: SystemTime) {
        if self.primary.as_ref().is_some_and(|l| l.core_id == f.track) {
            self.push_primary(f, now);
        } else if self.secondary.as_ref().is_some_and(|l| l.core_id == f.track) {
            self.push_secondary(f);
        }
    }

    fn push_primary(&mut self, f: &Frame, now: SystemTime) {
        let audio = self.primary_is_audio;
        let lane = self.primary.as_ref().expect("primary lane");
        let ts = i64::from(lane.timescale);
        let new = lane.sample(f, audio);

        if let Some(p) = lane.pending.as_ref() {
            let d = new.dts - p.dts;
            if d <= 0 || d > JUMP_SECS * ts {
                // Timestamps jumped: close what we have and restart on a
                // keyframe, marked as a discontinuity.
                self.flush();
                self.discontinuity_next = self.any_output;
                self.resume_from = None;
                self.waiting_key = true;
            } else {
                let mut p = self.primary.as_mut().unwrap().pending.take().unwrap();
                p.dur = d as u32;
                self.primary.as_mut().unwrap().last_dur = d as u32;
                self.commit_primary(p);
                let seg_len = new.dts - self.seg_start.unwrap_or(new.dts);
                let seg_min = self.segment_ms * ts / 1000;
                if new.key && seg_len >= seg_min {
                    self.close_chunk();
                    self.close_segment();
                }
            }
        }

        if self.seg_start.is_none() {
            if !new.key {
                self.waiting_key = true;
                return;
            }
            self.open_segment(new.dts, now);
        }
        self.primary.as_mut().unwrap().pending = Some(new);
    }

    fn push_secondary(&mut self, f: &Frame) {
        let lane = self.secondary.as_mut().expect("secondary lane");
        let new = lane.sample(f, true);
        if let Some(mut p) = lane.pending.take() {
            let d = new.dts - p.dts;
            p.dur =
                if d <= 0 || d > JUMP_SECS * i64::from(lane.timescale) { lane.fallback_dur(&p.data) } else { d as u32 };
            lane.last_dur = p.dur;
            self.queue.push_back(p);
        }
        self.secondary.as_mut().unwrap().pending = Some(new);
        if self.seg_start.is_none() {
            while self.queue.len() > 256 {
                self.queue.pop_front();
            }
        }
    }

    fn chunk_max(&self) -> i64 {
        let ts = i64::from(self.primary.as_ref().map_or(1, |l| l.timescale));
        (CHUNK_MS * ts / 1000).max(1)
    }

    fn chunk_span(&self) -> i64 {
        match (self.chunk.first(), self.chunk.last()) {
            (Some(a), Some(b)) => b.dts + i64::from(b.dur) - a.dts,
            _ => 0,
        }
    }

    fn commit_primary(&mut self, s: Sample) {
        if !self.chunk.is_empty() && s.dts + i64::from(s.dur) - self.chunk[0].dts > self.chunk_max() {
            self.close_chunk();
        }
        self.chunk.push(s);
    }

    fn open_segment(&mut self, dts: i64, now: SystemTime) {
        let lane = self.primary.as_ref().unwrap();
        let start_micros = lane.micros(dts);
        let mut discontinuity = std::mem::take(&mut self.discontinuity_next);
        if let Some(prev) = self.resume_from.take() {
            let gap = start_micros - prev;
            if !(0..=GAP_MICROS).contains(&gap) && self.any_output {
                discontinuity = true;
            }
        }
        if self.waiting_key {
            // Fresh start: audio from before this keyframe has no home.
            self.floor = Some(start_micros);
            self.waiting_key = false;
            if discontinuity || self.anchor.is_none() {
                self.anchor = Some((now, start_micros));
            }
        }
        let pdt = match self.anchor {
            Some((wall, at)) => {
                let d = start_micros - at;
                if d >= 0 { wall + Duration::from_micros(d as u64) } else { wall - Duration::from_micros((-d) as u64) }
            }
            None => now,
        };
        self.seg_start = Some(dts);
        self.seg_ticks = 0;
        self.seg_emitted = false;
        self.pending_open = Some(Out::Open { discontinuity, pdt });
    }

    fn close_chunk(&mut self) {
        if self.chunk.is_empty() {
            return;
        }
        let primary = self.primary.as_ref().unwrap();
        let end = self.chunk.last().map(|s| s.dts + i64::from(s.dur)).unwrap();
        let end_micros = primary.micros(end);

        let mut audio = Vec::new();
        if let Some(sec) = self.secondary.as_ref() {
            while let Some(s) = self.queue.front() {
                let start = sec.micros(s.dts);
                if start >= end_micros {
                    break;
                }
                let s = self.queue.pop_front().unwrap();
                if self.floor.is_some_and(|fl| start + sec.micros(i64::from(s.dur)) <= fl) {
                    continue;
                }
                audio.push(s);
            }
        }
        self.floor = None;

        self.fragment_seq = self.fragment_seq.wrapping_add(1);
        let mut runs = vec![Run { track_id: primary.track_id, samples: &self.chunk }];
        if let Some(sec) = self.secondary.as_ref() {
            runs.push(Run { track_id: sec.track_id, samples: &audio });
        }
        let data = fmp4::fragment(self.fragment_seq, &runs);
        self.seg_ticks += self.chunk_span();
        self.chunk.clear();
        if let Some(open) = self.pending_open.take() {
            self.out.push(open);
        }
        self.seg_emitted = true;
        self.any_output = true;
        self.out.push(Out::Fragment(data));
    }

    fn close_segment(&mut self) {
        self.seg_start = None;
        self.pending_open = None;
        if !std::mem::take(&mut self.seg_emitted) {
            return;
        }
        let ts = self.primary.as_ref().map_or(1, |l| l.timescale);
        self.out.push(Out::Close { duration: self.seg_ticks as f64 / f64::from(ts) });
    }

    /// Commits held-back frames with their last known duration and closes
    /// the open segment.
    fn flush(&mut self) {
        let pending = self.primary.as_mut().and_then(|lane| {
            let mut p = lane.pending.take()?;
            p.dur = lane.fallback_dur(&p.data);
            Some(p)
        });
        if let Some(p) = pending {
            let end_ticks = p.dts + i64::from(p.dur);
            self.commit_primary(p);
            self.resume_from = self.primary.as_ref().map(|l| l.micros(end_ticks));
        }
        if let Some(lane) = self.secondary.as_mut()
            && let Some(mut p) = lane.pending.take()
        {
            p.dur = lane.fallback_dur(&p.data);
            self.queue.push_back(p);
        }
        self.close_chunk();
        self.close_segment();
    }

    /// The recorder fell behind the live buffer and frames were skipped:
    /// close what we have; the next segment starts on a keyframe and is
    /// marked as a discontinuity.
    pub fn lagged(&mut self) {
        self.flush();
        self.queue.clear();
        self.waiting_key = true;
        self.discontinuity_next = self.any_output;
    }

    pub fn end(&mut self) {
        self.flush();
    }
}
