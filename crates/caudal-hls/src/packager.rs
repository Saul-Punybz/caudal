//! The LL-HLS segmenter for one stream: frames in, parts and segments out.
//!
//! Synchronous and clock-free (the caller passes the wall clock), so it is
//! driven the same way by the live task and by tests.
//!
//! Timeline rules:
//! - The *primary* track (video, or audio in an audio-only stream) decides
//!   every cut. A segment starts on a primary keyframe once the current one
//!   has run for `segment_ms`; it runs long if the GOP is longer.
//! - A part is closed as soon as adding the next primary frame would push
//!   it past `part_ms`, so no part exceeds PART-TARGET. Parts never cross a
//!   segment boundary.
//! - A frame's duration is only known when the next frame on its track
//!   arrives, so each track holds back one frame.
//! - The secondary track (audio next to video) is placed into whichever part
//!   is open when its sample *starts* before the part's end time.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::time::{Duration, SystemTime};

use bytes::{Bytes, BytesMut};
use caudal_core::{Codec, Cue, CueKind, Frame, TrackId, TrackInfo, TrackKind};

use crate::HlsConfig;
use caudal_cmaf::fmp4::{self, Mp4Track, Run, Sample};

/// Full segments kept in the playlist.
pub(crate) const WINDOW: usize = 6;
/// Segments at the live edge (counting the open one) that list their parts.
const PART_SEGMENTS: usize = 3;
/// Added to every decode time so small negative timestamps stay valid.
const SHIFT_SECS: i64 = 10;
/// A gap bigger than this between consecutive frames of one track is a
/// timestamp jump, not a frame duration.
const JUMP_SECS: i64 = 5;
/// After a lag, a gap bigger than this is marked as a discontinuity.
const GAP_MICROS: i64 = 500_000;

pub(crate) struct Part {
    /// Seconds, on the primary track's clock.
    pub duration: f64,
    pub independent: bool,
    pub data: Bytes,
}

pub(crate) struct Segment {
    pub msn: u64,
    pub parts: Vec<Part>,
    /// Wall clock at which the packager received the segment's first frame.
    pub pdt: SystemTime,
    /// Media time of the segment's first frame, on the `Cue::at_us` clock.
    pub start_us: i64,
    pub discontinuity: bool,
    /// Which init segment decodes it (see [`Packager::init_for`]).
    pub init_gen: u32,
    /// `Some` once the segment is complete: all its parts, concatenated.
    pub full: Option<Bytes>,
}

impl Segment {
    pub fn duration(&self) -> f64 {
        self.parts.iter().map(|p| p.duration).sum()
    }
}

/// What a request for a part or segment finds right now.
pub(crate) enum Lookup {
    Found(Bytes),
    /// Not there yet but it is the next thing to be produced: worth waiting.
    Pending,
    Gone,
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

    /// Duration to give a sample when there is no next one on its track to
    /// measure a gap against. Opus packets carry their own duration in the
    /// TOC byte (RFC 6716 §3.1), which can change frame to frame, so it is
    /// read off the packet itself instead of assuming the last one's length.
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

pub(crate) struct Packager {
    cfg: HlsConfig,
    primary: Option<Lane>,
    secondary: Option<Lane>,
    primary_is_audio: bool,
    /// The current init segment.
    pub init: Option<Bytes>,
    /// Generation of `init`; bumped each time the init segment changes.
    init_gen: u32,
    /// Older init segments that listed segments still need, oldest first.
    old_inits: VecDeque<(u32, Bytes)>,
    /// The publisher left and a republish may still continue this playlist:
    /// no `#EXT-X-ENDLIST` yet.
    pub suspended: bool,

    /// Primary samples committed to the open part.
    part: Vec<Sample>,
    /// Secondary samples committed, waiting for a part to close.
    queue: VecDeque<Sample>,
    /// Primary dts at which the open segment began.
    seg_start: Option<i64>,
    /// Secondary samples starting before this (primary clock, shifted) are
    /// dropped: they precede the first segment.
    floor: Option<i64>,
    waiting_key: bool,
    /// Primary end time (micros) before a lag, to measure the gap after it.
    resume_from: Option<i64>,
    discontinuity_next: bool,
    fragment_seq: u32,

    pub segments: VecDeque<Segment>,
    next_msn: u64,
    pub discontinuity_seq: u64,
    target: u64,
    pub ended: bool,
    /// `CODECS` and `RESOLUTION` for the multivariant playlist.
    codecs: Vec<String>,
    resolution: Option<(u32, u32)>,
    frame_rate: Option<f64>,

    /// `EXT-X-DATERANGE` lines for SCTE-35 cues, oldest first.
    dateranges: VecDeque<DateRange>,
    /// Cues that arrived before the first segment: nothing to date them by.
    early_cues: Vec<Cue>,
    /// The splice-out still waiting for its splice-in, which reuses its ID.
    open_out: Option<OpenOut>,
    cue_seq: u64,
    /// Ad breaks for the legacy `EXT-X-CUE-OUT-CONT` lines (only with
    /// `cue_out_tags`), oldest first.
    breaks: VecDeque<Break>,
}

/// One ad break, for writing `EXT-X-CUE-OUT-CONT` on the segments inside it.
struct Break {
    out_us: i64,
    planned_us: Option<i64>,
    in_us: Option<i64>,
}

impl Break {
    /// Ends at its splice-in; without one, when its planned duration runs
    /// out; with neither, it stays open until the next init or splice-in.
    fn end_us(&self) -> i64 {
        self.in_us.unwrap_or_else(|| self.planned_us.map_or(i64::MAX, |d| self.out_us + d))
    }
}

/// One `EXT-X-DATERANGE` line. The text is fixed when the cue arrives:
/// tags with the same ID must keep the same attribute values on every
/// reload (RFC 8216bis §4.4.5.1).
struct DateRange {
    /// Media time the tag is placed at (after the PDT of the segment that
    /// contains it).
    at_us: i64,
    /// Media time the range ends, for keeping it while it still overlaps
    /// the window.
    end_us: i64,
    line: String,
}

struct OpenOut {
    id: String,
    start_date: String,
    at_us: i64,
}

/// Everything one rendition contributes to a multivariant playlist's
/// `#EXT-X-STREAM-INF` line. `None` from [`Packager::variant_attrs`] until
/// the tracks are known.
pub(crate) struct VariantAttrs {
    pub codecs: String,
    pub resolution: Option<(u32, u32)>,
    pub frame_rate: Option<f64>,
    pub peak: u64,
    pub average: Option<u64>,
}

impl Packager {
    pub fn new(cfg: HlsConfig) -> Self {
        Self {
            cfg,
            primary: None,
            secondary: None,
            primary_is_audio: false,
            init: None,
            init_gen: 0,
            old_inits: VecDeque::new(),
            suspended: false,
            part: Vec::new(),
            queue: VecDeque::new(),
            seg_start: None,
            floor: None,
            waiting_key: true,
            resume_from: None,
            discontinuity_next: false,
            fragment_seq: 0,
            segments: VecDeque::new(),
            next_msn: 0,
            discontinuity_seq: 0,
            target: u64::from(cfg.segment_ms).div_ceil(1000).max(1),
            ended: false,
            codecs: Vec::new(),
            resolution: None,
            frame_rate: None,
            dateranges: VecDeque::new(),
            breaks: VecDeque::new(),
            early_cues: Vec::new(),
            open_out: None,
            cue_seq: 0,
        }
    }

    /// Turns an SCTE-35 cue into `EXT-X-DATERANGE` (RFC 8216 §4.3.2.7.1):
    /// a splice-out gets `SCTE35-OUT` and `PLANNED-DURATION`; the next
    /// splice-in reuses its ID with `SCTE35-IN` and the measured `DURATION`;
    /// anything else gets `SCTE35-CMD`. `START-DATE` is the wall clock of
    /// the segment the cue falls in, plus the cue's media offset into it.
    pub fn push_cue(&mut self, cue: &Cue) {
        if (!self.cfg.cue_tags && !self.cfg.cue_out_tags) || self.ended {
            return;
        }
        let anchor = self.segments.iter().rev().find(|s| s.start_us <= cue.at_us).or(self.segments.front());
        let Some(anchor) = anchor else {
            if self.early_cues.len() < 16 {
                self.early_cues.push(cue.clone());
            }
            return;
        };
        let offset = cue.at_us - anchor.start_us;
        let date = if offset >= 0 {
            anchor.pdt + Duration::from_micros(offset as u64)
        } else {
            anchor.pdt - Duration::from_micros(offset.unsigned_abs())
        };
        let date = rfc3339(date);
        let hex = caudal_scte35::to_hex(&cue.section);
        let (line, end_us) = match cue.kind {
            CueKind::Out { duration_us } => {
                let id = self.next_cue_id();
                let mut line = format!("#EXT-X-DATERANGE:ID=\"{id}\",START-DATE=\"{date}\"");
                if let Some(d) = duration_us.filter(|d| *d > 0) {
                    let _ = write!(line, ",PLANNED-DURATION={:.3}", d as f64 / 1e6);
                }
                let _ = write!(line, ",SCTE35-OUT={hex}");
                self.open_out = Some(OpenOut { id, start_date: date, at_us: cue.at_us });
                (line, cue.at_us + duration_us.unwrap_or(0).max(0))
            }
            CueKind::In => match self.open_out.take() {
                Some(out) => {
                    let d = (cue.at_us - out.at_us).max(0) as f64 / 1e6;
                    let line = format!(
                        "#EXT-X-DATERANGE:ID=\"{}\",START-DATE=\"{}\",DURATION={d:.3},SCTE35-IN={hex}",
                        out.id, out.start_date
                    );
                    (line, cue.at_us)
                }
                None => {
                    let id = self.next_cue_id();
                    (format!("#EXT-X-DATERANGE:ID=\"{id}\",START-DATE=\"{date}\",SCTE35-IN={hex}"), cue.at_us)
                }
            },
            CueKind::Other => {
                let id = self.next_cue_id();
                (format!("#EXT-X-DATERANGE:ID=\"{id}\",START-DATE=\"{date}\",SCTE35-CMD={hex}"), cue.at_us)
            }
        };
        if self.cfg.cue_tags {
            self.dateranges.push_back(DateRange { at_us: cue.at_us, end_us, line });
        }
        if self.cfg.cue_out_tags {
            self.push_legacy_cue(cue);
        }
        self.prune_dateranges();
    }

    /// Legacy tags, placed like the DATERANGE lines (after the PDT of the
    /// segment the cue falls in): `EXT-X-CUE-OUT[:DURATION=s]` at a
    /// splice-out, `EXT-X-CUE-IN` at the splice-in. The segments in between
    /// get `EXT-X-CUE-OUT-CONT` when the playlist is written.
    fn push_legacy_cue(&mut self, cue: &Cue) {
        match cue.kind {
            CueKind::Out { duration_us } => {
                let planned_us = duration_us.filter(|d| *d > 0);
                let line = match planned_us {
                    Some(d) => format!("#EXT-X-CUE-OUT:DURATION={:.3}", d as f64 / 1e6),
                    None => "#EXT-X-CUE-OUT".to_owned(),
                };
                let end_us = cue.at_us + planned_us.unwrap_or(0);
                self.dateranges.push_back(DateRange { at_us: cue.at_us, end_us, line });
                self.breaks.push_back(Break { out_us: cue.at_us, planned_us, in_us: None });
            }
            CueKind::In => {
                self.dateranges.push_back(DateRange {
                    at_us: cue.at_us,
                    end_us: cue.at_us,
                    line: "#EXT-X-CUE-IN".to_owned(),
                });
                if let Some(b) = self.breaks.back_mut().filter(|b| b.in_us.is_none()) {
                    b.in_us = Some(cue.at_us);
                }
            }
            CueKind::Other => {}
        }
    }

    fn next_cue_id(&mut self) -> String {
        self.cue_seq += 1;
        format!("scte35-{}", self.cue_seq)
    }

    /// Drops ranges that ended before the oldest segment in the window.
    fn prune_dateranges(&mut self) {
        let Some(first) = self.segments.front().map(|s| s.start_us) else { return };
        self.dateranges.retain(|d| d.at_us.max(d.end_us) >= first);
        self.breaks.retain(|b| b.end_us() >= first);
    }

    pub fn part_target(&self) -> f64 {
        f64::from(self.cfg.part_ms.max(1)) / 1000.0
    }

    pub fn target_duration(&self) -> u64 {
        self.target
    }

    /// The track list changed. Returns true if the output was reset.
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
            // One config did not parse: fall back to the track that does.
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

        // A new init segment: what was cut before is still listed, under
        // its own `EXT-X-MAP`, after a discontinuity.
        self.flush();
        let had_output = !self.segments.is_empty();
        if init != self.init {
            if had_output {
                if let Some(old) = self.init.take() {
                    self.old_inits.push_back((self.init_gen, old));
                }
                self.init_gen += 1;
            } else {
                // Nothing was ever listed: no player can hold the old one.
                self.old_inits.clear();
            }
        }
        self.dateranges.clear();
        self.breaks.clear();
        self.open_out = None;
        self.discontinuity_next = had_output;
        self.resume_from = None;
        self.queue.clear();
        self.floor = None;
        self.waiting_key = true;
        self.init = init;
        self.codecs = mp4.iter().filter_map(|t| codec_string(&t.info)).collect();
        self.resolution =
            mp4.iter().find_map(|t| t.info.video.map(|v| (v.width, v.height))).filter(|&(w, h)| w > 0 && h > 0);
        self.frame_rate = mp4.iter().find_map(|t| t.info.video.and_then(|v| v.fps)).filter(|fps| *fps > 0.0);
        let mut lanes = mp4.iter().map(|t| Lane::new(t.track_id, &t.info));
        self.primary = lanes.next();
        self.secondary = lanes.next();
        self.primary_is_audio = mp4.first().is_some_and(|t| t.info.kind() == TrackKind::Audio);
        if self.init.is_none() {
            self.primary = None;
            self.secondary = None;
        }
        true
    }

    /// Feeds one frame. `now` is the wall clock at which it was received.
    pub fn push(&mut self, f: &Frame, now: SystemTime) {
        if self.ended {
            return;
        }
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
                self.discontinuity_next = !self.segments.is_empty();
                self.waiting_key = true;
            } else {
                let mut p = self.primary.as_mut().unwrap().pending.take().unwrap();
                p.dur = d as u32;
                self.primary.as_mut().unwrap().last_dur = d as u32;
                self.commit_primary(p);
                let seg_len = new.dts - self.seg_start.unwrap_or(new.dts);
                let seg_min = i64::from(self.cfg.segment_ms) * ts / 1000;
                if new.key && seg_len >= seg_min {
                    self.close_part();
                    self.close_segment();
                } else if !self.part.is_empty() && self.part_span() + d > self.part_max() {
                    self.close_part();
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
        // No segment to put audio in yet (waiting for a video keyframe):
        // keep only a short tail.
        if self.seg_start.is_none() {
            while self.queue.len() > 256 {
                self.queue.pop_front();
            }
        }
    }

    fn commit_primary(&mut self, s: Sample) {
        // Never let a part grow past the part target.
        if !self.part.is_empty() && s.dts + i64::from(s.dur) - self.part[0].dts > self.part_max() {
            self.close_part();
        }
        self.part.push(s);
    }

    fn part_span(&self) -> i64 {
        match (self.part.first(), self.part.last()) {
            (Some(a), Some(b)) => b.dts + i64::from(b.dur) - a.dts,
            _ => 0,
        }
    }

    fn part_max(&self) -> i64 {
        let ts = i64::from(self.primary.as_ref().map_or(1, |l| l.timescale));
        (i64::from(self.cfg.part_ms) * ts / 1000).max(1)
    }

    fn open_segment(&mut self, dts: i64, now: SystemTime) {
        let lane = self.primary.as_ref().unwrap();
        let start_micros = lane.micros(dts);
        // Sample times carry SHIFT_SECS; cue times do not.
        let media_us = lane.micros(dts - lane.shift);
        let mut discontinuity = std::mem::take(&mut self.discontinuity_next);
        if let Some(prev) = self.resume_from.take() {
            let gap = start_micros - prev;
            if !(0..=GAP_MICROS).contains(&gap) && !self.segments.is_empty() {
                discontinuity = true;
            }
        }
        if self.waiting_key {
            // Fresh start: audio from before this keyframe has no home.
            self.floor = Some(start_micros);
            self.waiting_key = false;
        }
        self.seg_start = Some(dts);
        self.segments.push_back(Segment {
            msn: self.next_msn,
            parts: Vec::new(),
            pdt: now,
            start_us: media_us,
            discontinuity,
            init_gen: self.init_gen,
            full: None,
        });
        self.next_msn += 1;
        for cue in std::mem::take(&mut self.early_cues) {
            self.push_cue(&cue);
        }
    }

    fn close_part(&mut self) {
        if self.part.is_empty() {
            return;
        }
        let primary = self.primary.as_ref().unwrap();
        let end = self.part.last().map(|s| s.dts + i64::from(s.dur)).unwrap();
        let end_micros = primary.micros(end);
        let duration = self.part_span() as f64 / f64::from(primary.timescale);

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
        let mut runs = vec![Run { track_id: primary.track_id, samples: &self.part }];
        if let Some(sec) = self.secondary.as_ref() {
            runs.push(Run { track_id: sec.track_id, samples: &audio });
        }
        let data = fmp4::fragment(self.fragment_seq, &runs);
        let independent = self.part[0].key;
        self.part.clear();
        if let Some(seg) = self.segments.back_mut().filter(|s| s.full.is_none()) {
            seg.parts.push(Part { duration, independent, data });
        }
    }

    fn close_segment(&mut self) {
        self.seg_start = None;
        let Some(seg) = self.segments.back_mut().filter(|s| s.full.is_none()) else { return };
        if seg.parts.is_empty() {
            self.segments.pop_back();
            self.next_msn -= 1;
            return;
        }
        let mut full = BytesMut::with_capacity(seg.parts.iter().map(|p| p.data.len()).sum());
        for p in &seg.parts {
            full.extend_from_slice(&p.data);
        }
        seg.full = Some(full.freeze());
        self.target = self.target.max(seg.duration().round() as u64);
        while self.segments.iter().filter(|s| s.full.is_some()).count() > WINDOW {
            if let Some(old) = self.segments.pop_front()
                && old.discontinuity
            {
                self.discontinuity_seq += 1;
            }
        }
        let first_gen = self.segments.front().map_or(self.init_gen, |s| s.init_gen);
        self.old_inits.retain(|(g, _)| *g >= first_gen);
        self.prune_dateranges();
    }

    /// Commits held-back frames with their last known duration and closes
    /// the open part and segment.
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
        self.close_part();
        self.close_segment();
    }

    /// The viewer fell behind the live buffer and frames were skipped.
    pub fn lagged(&mut self) {
        self.flush();
        self.queue.clear();
        self.waiting_key = true;
    }

    pub fn end(&mut self) {
        self.flush();
        self.suspended = false;
        self.ended = true;
    }

    /// The publisher left, but may come back under the same name: close
    /// what is open and keep the playlist live (no `EXT-X-ENDLIST`), with
    /// the preload hint still pointing at the next part.
    pub fn suspend(&mut self) {
        self.flush();
        self.suspended = true;
    }

    /// A new publisher took over the stream. Media sequence numbers keep
    /// counting; its first segment starts on a keyframe after an
    /// `EXT-X-DISCONTINUITY` (its timestamps start over).
    pub fn resume(&mut self) {
        self.flush();
        self.suspended = false;
        self.discontinuity_next = !self.segments.is_empty();
        self.resume_from = None;
        self.queue.clear();
        self.floor = None;
        self.waiting_key = true;
        // Cue times are on the old publisher's clock.
        self.dateranges.clear();
        self.breaks.clear();
        self.open_out = None;
        self.early_cues.clear();
    }

    /// The init segment of generation `gen`, while anything listed (or the
    /// open segment) still needs it.
    pub fn init_for(&self, generation: u32) -> Option<Bytes> {
        if generation == self.init_gen {
            return self.init.clone();
        }
        self.old_inits.iter().find(|(g, _)| *g == generation).map(|(_, b)| b.clone())
    }

    /// `(msn, part index)` of the newest complete part.
    pub fn last_part(&self) -> Option<(u64, usize)> {
        self.segments.iter().rev().find(|s| !s.parts.is_empty()).map(|s| (s.msn, s.parts.len() - 1))
    }

    /// `(msn, part index)` the next part will have.
    pub fn next_part(&self) -> (u64, usize) {
        match self.segments.back() {
            Some(s) if s.full.is_none() => (s.msn, s.parts.len()),
            _ => (self.next_msn, 0),
        }
    }

    /// Msn of the newest complete segment.
    pub fn last_complete(&self) -> Option<u64> {
        self.segments.iter().rev().find(|s| s.full.is_some()).map(|s| s.msn)
    }

    fn segment(&self, msn: u64) -> Option<&Segment> {
        let first = self.segments.front()?.msn;
        self.segments.get(msn.checked_sub(first)? as usize)
    }

    pub fn lookup(&self, msn: u64, part: Option<usize>) -> Lookup {
        let pending = |(nm, np): (u64, usize)| -> bool {
            !self.ended
                && match part {
                    Some(p) => (msn, p) <= (nm, np),
                    None => msn <= nm,
                }
        };
        if let Some(seg) = self.segment(msn) {
            let found = match part {
                Some(i) => seg.parts.get(i).map(|p| p.data.clone()),
                None => seg.full.clone(),
            };
            if let Some(b) = found {
                return Lookup::Found(b);
            }
            if seg.full.is_none() && pending(self.next_part()) {
                return Lookup::Pending;
            }
            return Lookup::Gone;
        }
        if msn >= self.next_msn && pending(self.next_part()) {
            return Lookup::Pending;
        }
        Lookup::Gone
    }

    /// True once there is something worth putting in a playlist.
    pub fn ready(&self) -> bool {
        self.init.is_some() && self.last_part().is_some()
    }

    pub fn playlist(&self) -> String {
        let pt = self.part_target();
        let mut o = String::with_capacity(4096);
        o.push_str("#EXTM3U\n#EXT-X-VERSION:9\n");
        let _ = writeln!(o, "#EXT-X-TARGETDURATION:{}", self.target);
        // At least 3x the part target (RFC 8216bis 4.4.3.8); the extra
        // millisecond keeps float rounding from landing a hair under it,
        // which Apple's validator flags (-50102).
        let _ = writeln!(o, "#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK={:.3}", pt * 3.0 + 0.001);
        let _ = writeln!(o, "#EXT-X-PART-INF:PART-TARGET={pt:.3}");
        let first = self.segments.front().map_or(self.next_msn, |s| s.msn);
        let _ = writeln!(o, "#EXT-X-MEDIA-SEQUENCE:{first}");
        if self.discontinuity_seq > 0 {
            let _ = writeln!(o, "#EXT-X-DISCONTINUITY-SEQUENCE:{}", self.discontinuity_seq);
        }
        o.push_str("#EXT-X-INDEPENDENT-SEGMENTS\n");
        let n = self.segments.len();
        let mut map_gen = self.segments.iter().find(|s| !s.parts.is_empty()).map_or(self.init_gen, |s| s.init_gen);
        let _ = writeln!(o, "#EXT-X-MAP:URI=\"{}\"", init_uri(map_gen));
        // Each DATERANGE goes right after the PDT of the listed segment its
        // cue falls in (the first listed one for anything older).
        let listed: Vec<usize> = (0..n).filter(|&i| !self.segments[i].parts.is_empty()).collect();
        for (k, &i) in listed.iter().enumerate() {
            let seg = &self.segments[i];
            if seg.discontinuity {
                o.push_str("#EXT-X-DISCONTINUITY\n");
            }
            if seg.init_gen != map_gen {
                map_gen = seg.init_gen;
                let _ = writeln!(o, "#EXT-X-MAP:URI=\"{}\"", init_uri(map_gen));
            }
            let _ = writeln!(o, "#EXT-X-PROGRAM-DATE-TIME:{}", rfc3339(seg.pdt));
            let from = if k == 0 { i64::MIN } else { seg.start_us };
            let until = listed.get(k + 1).map_or(i64::MAX, |&j| self.segments[j].start_us);
            // A segment that starts inside an ad break (after its CUE-OUT,
            // before its CUE-IN) carries the legacy continuation tag.
            if let Some(b) = self.breaks.iter().find(|b| b.out_us < seg.start_us && seg.start_us < b.end_us()) {
                let elapsed = (seg.start_us - b.out_us) as f64 / 1e6;
                match b.planned_us {
                    Some(d) => {
                        let _ =
                            writeln!(o, "#EXT-X-CUE-OUT-CONT:ElapsedTime={elapsed:.3},Duration={:.3}", d as f64 / 1e6);
                    }
                    None => {
                        let _ = writeln!(o, "#EXT-X-CUE-OUT-CONT:ElapsedTime={elapsed:.3}");
                    }
                }
            }
            for d in self.dateranges.iter().filter(|d| (from..until).contains(&d.at_us)) {
                o.push_str(&d.line);
                o.push('\n');
            }
            if i + PART_SEGMENTS >= n {
                for (j, p) in seg.parts.iter().enumerate() {
                    let _ = write!(o, "#EXT-X-PART:DURATION={:.5},URI=\"s{}.p{}.m4s\"", p.duration, seg.msn, j);
                    o.push_str(if p.independent { ",INDEPENDENT=YES\n" } else { "\n" });
                }
            }
            if seg.full.is_some() {
                let _ = writeln!(o, "#EXTINF:{:.5},\ns{}.m4s", seg.duration(), seg.msn);
            }
        }
        if self.ended {
            o.push_str("#EXT-X-ENDLIST\n");
        } else {
            let (m, p) = self.next_part();
            let _ = writeln!(o, "#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"s{m}.p{p}.m4s\"");
        }
        o
    }
}

impl Packager {
    /// Everything one variant contributes to a multivariant playlist:
    /// `CODECS`, `RESOLUTION`, `FRAME-RATE` and measured `BANDWIDTH` /
    /// `AVERAGE-BANDWIDTH`. `None` until the tracks are known (players enter
    /// through the multivariant playlist, so callers wait on this).
    pub fn variant_attrs(&self) -> Option<VariantAttrs> {
        if self.codecs.is_empty() {
            return None;
        }
        let (peak, average) = self.bitrates();
        Some(VariantAttrs {
            codecs: self.codecs.join(","),
            resolution: self.resolution,
            frame_rate: self.frame_rate,
            peak,
            average,
        })
    }

    /// Peak and average bits per second over the completed segments in the
    /// window. Peak has a floor so a fresh stream still advertises something
    /// sane; average is `None` until at least one segment has completed.
    fn bitrates(&self) -> (u64, Option<u64>) {
        let full: Vec<(f64, f64)> = self
            .segments
            .iter()
            .filter_map(|s| s.full.as_ref().map(|b| (b.len() as f64 * 8.0, s.duration().max(0.001))))
            .collect();
        let peak = full.iter().map(|&(bits, dur)| (bits / dur) as u64).max().unwrap_or(0).max(64_000);
        let average = (!full.is_empty()).then(|| {
            let (bits, dur) = full.iter().fold((0.0, 0.0), |(b, d), &(bi, du)| (b + bi, d + du));
            (bits / dur.max(0.001)) as u64
        });
        (peak, average)
    }
}

/// `init.mp4` for the first init segment, `init{gen}.mp4` for later ones,
/// so a URI never changes meaning.
pub(crate) fn init_uri(generation: u32) -> String {
    if generation == 0 { "init.mp4".to_owned() } else { format!("init{generation}.mp4") }
}

/// RFC 6381 codec string from a track's configuration record.
pub(crate) fn codec_string(t: &TrackInfo) -> Option<String> {
    let c = &t.init;
    match t.codec {
        // avcC: [version, profile, compatibility, level, ...]
        Codec::H264 if c.len() >= 4 => Some(format!("avc1.{:02x}{:02x}{:02x}", c[1], c[2], c[3])),
        // hvcC: [version, space|tier|profile, compat(4), constraints(6), level, ...]
        Codec::H265 if c.len() >= 13 => {
            let space = ["", "A", "B", "C"][usize::from(c[1] >> 6)];
            let tier = if c[1] & 0x20 != 0 { 'H' } else { 'L' };
            let profile = c[1] & 0x1f;
            let compat = u32::from_be_bytes([c[2], c[3], c[4], c[5]]).reverse_bits();
            let mut cons: Vec<String> = c[6..12].iter().map(|b| format!("{b:X}")).collect();
            while cons.len() > 1 && cons.last().is_some_and(|b| b == "0") {
                cons.pop();
            }
            Some(format!("hvc1.{space}{profile}.{compat:X}.{tier}{}.{}", c[12], cons.join(".")))
        }
        // AudioSpecificConfig: object type in the top 5 bits.
        Codec::Aac if !c.is_empty() => Some(format!("mp4a.40.{}", c[0] >> 3)),
        // RFC 6381 (and common practice): Opus in MP4 is just "opus", no
        // profile or level suffix.
        Codec::Opus if caudal_cmaf::opus::parse_opus_head(c).is_some() => Some("opus".to_string()),
        _ => None,
    }
}

/// `2026-09-17T21:38:00.123Z`, without a date crate.
pub(crate) fn rfc3339(t: SystemTime) -> String {
    let d = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let ms = d.subsec_millis();
    let (days, sod) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{ms:03}Z", sod / 3600, sod % 3600 / 60, sod % 60)
}
