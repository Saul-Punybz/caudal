//! The classic-HLS segmenter: frames in, MPEG-TS segments and a version 3
//! media playlist out (`/hls/{name}/ts.m3u8`, segments `t{seq}.ts`).
//!
//! For players that only know HLS with TS segments (older set-top boxes,
//! smart TVs and IPTV middleware), and for codecs fMP4 cannot carry here:
//! MPEG-2 video and MP2 audio from a broadcast feed go out as they came in.
//!
//! One [`TsMux`] (media-clock mode) muxes the whole stream, so continuity
//! counters and PTS/DTS/PCR run on unbroken from one segment to the next;
//! the segmenter only decides where to cut its output. Rules:
//! - The primary track (video, or audio in an audio-only stream) decides
//!   every cut: a segment ends at the first primary keyframe at least
//!   `segment_ms` after its own start, and runs long if the GOP is longer.
//! - Every segment starts with PAT/PMT, then that keyframe, so it decodes
//!   on its own. Frames before the first keyframe are dropped.
//! - A timestamp jump, a lag, a track change or a publisher reconnect
//!   closes the open segment; the next one starts on a keyframe, after an
//!   `EXT-X-DISCONTINUITY`.
//!
//! Synchronous and clock-free, like [`crate::packager`], so the live task
//! and the tests drive it the same way.

use std::collections::VecDeque;
use std::fmt::Write as _;

use bytes::Bytes;
use caudal_core::{Codec, Cue, Frame, TrackId, TrackInfo, TrackKind};
use caudal_ts::mux::TsMux;

/// Segments listed in the playlist.
pub(crate) const TS_WINDOW: usize = 6;
/// Segments kept past the window, so a player that read a playlist just
/// before it slid can still fetch what it listed.
const TS_KEEP: usize = 2;
/// Added to every timestamp so small negative ones (B-frames at the start
/// of a stream) stay positive on the 33-bit TS clock.
const SHIFT_SECS: i64 = 10;
/// A gap bigger than this between consecutive primary frames is a
/// timestamp jump, not a frame duration.
const JUMP_SECS: i64 = 5;
/// An open segment that grows past this without a keyframe to cut on (a
/// broken encoder with no GOP) is closed anyway, so memory stays bounded.
/// The next segment then starts mid-GOP; it still starts with PAT/PMT.
const MAX_SEGMENT_BYTES: usize = 64 << 20;

pub(crate) struct TsSegment {
    pub seq: u64,
    /// Seconds, on the primary track's clock.
    pub duration: f64,
    pub discontinuity: bool,
    pub data: Bytes,
}

struct Open {
    /// Primary dts (shifted) at which it began.
    start: i64,
    discontinuity: bool,
    data: Vec<u8>,
}

#[derive(Clone, Copy)]
struct Primary {
    id: TrackId,
    timescale: i64,
}

pub(crate) struct TsSegmenter {
    segment_ms: u32,
    mux: TsMux,
    tracks: Vec<TrackInfo>,
    /// What the current tracks look like, to tell a real change from a
    /// repeated `TracksChanged`.
    signature: Vec<(TrackId, Codec, Bytes)>,
    primary: Option<Primary>,
    open: Option<Open>,
    /// The last primary frame's dts (shifted) and the gap before it.
    last_dts: Option<i64>,
    last_dur: i64,
    discontinuity_next: bool,
    pub segments: VecDeque<TsSegment>,
    next_seq: u64,
    /// Discontinuities on segments that are no longer held.
    dropped_discontinuities: u64,
    /// `EXT-X-TARGETDURATION`: never shrinks, since it may not change while
    /// the playlist lives (RFC 8216 §6.2.1); grows only for a GOP longer
    /// than every one before it.
    target: u64,
    pub ended: bool,
}

impl TsSegmenter {
    pub fn new(segment_ms: u32) -> Self {
        Self {
            segment_ms: segment_ms.max(1),
            mux: TsMux::with_media_clock(),
            tracks: Vec::new(),
            signature: Vec::new(),
            primary: None,
            open: None,
            last_dts: None,
            last_dur: 0,
            discontinuity_next: false,
            segments: VecDeque::new(),
            next_seq: 0,
            dropped_discontinuities: 0,
            target: u64::from(segment_ms.max(1)).div_ceil(1000),
            ended: false,
        }
    }

    /// The sequence number the open (or next) segment gets.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    pub fn target_duration(&self) -> u64 {
        self.target
    }

    /// The track list changed. A repeat of the same tracks is a no-op; a
    /// real change closes the open segment and restarts on a keyframe after
    /// a discontinuity.
    pub fn set_tracks(&mut self, tracks: &[TrackInfo]) {
        let signature: Vec<_> = tracks.iter().map(|t| (t.id, t.codec, t.init.clone())).collect();
        if signature == self.signature {
            return;
        }
        self.break_here();
        self.signature = signature;
        self.tracks = tracks.to_vec();
        self.mux.set_tracks(tracks);
        let video = |t: &&TrackInfo| matches!(t.codec, Codec::H264 | Codec::H265 | Codec::Mpeg2Video);
        let audio = |t: &&TrackInfo| matches!(t.codec, Codec::Aac | Codec::Mp2 | Codec::Mp3);
        let primary = if self.mux.has_video() {
            tracks.iter().rfind(video)
        } else if self.mux.has_audio() {
            tracks.iter().rfind(audio)
        } else {
            None
        };
        self.primary = primary.map(|t| Primary { id: t.id, timescale: i64::from(t.timescale.max(1)) });
    }

    /// Feeds one frame.
    pub fn push(&mut self, f: &Frame) {
        if self.ended {
            return;
        }
        let Some(primary) = self.primary else { return };
        let Some(idx) = self.tracks.iter().position(|t| t.id == f.track) else { return };
        let (timescale, kind) = (self.tracks[idx].timescale, self.tracks[idx].kind());
        let shift = SHIFT_SECS * i64::from(timescale.max(1));
        let shifted = Frame { dts: f.dts + shift, pts: f.pts + shift, ..f.clone() };
        if f.track == primary.id {
            let key = f.keyframe || kind == TrackKind::Audio;
            let dts = shifted.dts;
            if let Some(last) = self.last_dts {
                let d = dts - last;
                if d <= 0 || d > JUMP_SECS * primary.timescale {
                    // Timestamps jumped: what came before is one segment,
                    // what follows starts on a keyframe after a break.
                    self.break_here();
                } else {
                    self.last_dur = d;
                    let cut = self.open.as_ref().is_some_and(|o| {
                        let min = i64::from(self.segment_ms) * primary.timescale / 1000;
                        (key && dts - o.start >= min) || o.data.len() >= MAX_SEGMENT_BYTES
                    });
                    if cut {
                        self.close(dts);
                    }
                }
            }
            if self.open.is_none() {
                // The first segment, or the one after a break, must start
                // on a keyframe; one cut for size may not.
                if !key && self.last_dts.is_none() {
                    return;
                }
                self.mux.force_psi();
                self.open = Some(Open {
                    start: dts,
                    discontinuity: std::mem::take(&mut self.discontinuity_next),
                    data: Vec::new(),
                });
            }
            self.last_dts = Some(dts);
        } else if self.open.is_none() {
            // No segment to put it in yet: it precedes the first keyframe.
            return;
        }
        self.mux.push_frame(&self.tracks[idx], &shifted);
        self.collect();
    }

    /// An SCTE-35 cue: carried on its own PID, in whichever segment is
    /// open when it arrives.
    pub fn push_cue(&mut self, cue: &Cue) {
        if self.ended || self.open.is_none() {
            return;
        }
        self.mux.push_cue(cue);
        self.collect();
    }

    /// The subscriber lagged and was moved forward to a keyframe.
    pub fn lagged(&mut self) {
        self.break_here();
    }

    /// The publisher left; a republish may continue this playlist after a
    /// discontinuity.
    pub fn suspend(&mut self) {
        self.break_here();
        // The next publish sends its tracks again; take them even if they
        // look the same, so the mux writes their PSI afresh.
        self.signature.clear();
    }

    /// The stream is over: the last segment closes and the playlist ends.
    pub fn end(&mut self) {
        self.suspend();
        self.ended = true;
    }

    /// Nobody asked for this rendition for a while: drop every segment but
    /// keep counting, so a player that comes back sees media sequence
    /// numbers that only go up, after a discontinuity.
    pub fn idle(&mut self) {
        self.suspend();
        self.dropped_discontinuities += self.segments.iter().filter(|s| s.discontinuity).count() as u64;
        self.segments.clear();
        self.discontinuity_next = self.next_seq > 0;
    }

    /// The segment `seq`, if it is still held.
    pub fn segment(&self, seq: u64) -> Option<Bytes> {
        self.segments.iter().find(|s| s.seq == seq).map(|s| s.data.clone())
    }

    /// The version 3 media playlist (RFC 8216): the last [`TS_WINDOW`]
    /// complete segments, `EXT-X-ENDLIST` once the stream has ended.
    pub fn playlist(&self) -> String {
        let skip = self.segments.len().saturating_sub(TS_WINDOW);
        let listed = self.segments.iter().skip(skip);
        let first = self.segments.get(skip).map_or(self.next_seq, |s| s.seq);
        let discontinuity_seq =
            self.dropped_discontinuities + self.segments.iter().take(skip).filter(|s| s.discontinuity).count() as u64;
        let mut o = String::from("#EXTM3U\n#EXT-X-VERSION:3\n");
        let _ = writeln!(o, "#EXT-X-TARGETDURATION:{}", self.target);
        let _ = writeln!(o, "#EXT-X-MEDIA-SEQUENCE:{first}");
        if discontinuity_seq > 0 {
            let _ = writeln!(o, "#EXT-X-DISCONTINUITY-SEQUENCE:{discontinuity_seq}");
        }
        for s in listed {
            if s.discontinuity {
                o.push_str("#EXT-X-DISCONTINUITY\n");
            }
            let _ = writeln!(o, "#EXTINF:{:.3},\nt{}.ts", s.duration, s.seq);
        }
        if self.ended {
            o.push_str("#EXT-X-ENDLIST\n");
        }
        o
    }

    /// Closes the open segment at the last frame seen, and makes the next
    /// segment start on a keyframe after a discontinuity.
    fn break_here(&mut self) {
        if let Some(last) = self.last_dts
            && self.open.is_some()
        {
            self.close(last + self.last_dur.max(1));
        }
        self.open = None;
        self.last_dts = None;
        self.discontinuity_next = self.discontinuity_next || !self.segments.is_empty();
    }

    fn collect(&mut self) {
        let out = self.mux.take_output();
        if let Some(o) = self.open.as_mut() {
            o.data.extend_from_slice(&out);
        }
    }

    /// Closes the open segment, which ends at primary dts `end` (shifted).
    fn close(&mut self, end: i64) {
        let Some(open) = self.open.take() else { return };
        let Some(primary) = self.primary else { return };
        if open.data.is_empty() {
            self.discontinuity_next |= open.discontinuity;
            return;
        }
        let duration = (end - open.start).max(0) as f64 / primary.timescale as f64;
        self.target = self.target.max(duration.ceil() as u64).max(1);
        self.segments.push_back(TsSegment {
            seq: self.next_seq,
            duration,
            discontinuity: open.discontinuity,
            data: Bytes::from(open.data),
        });
        self.next_seq += 1;
        while self.segments.len() > TS_WINDOW + TS_KEEP {
            if let Some(s) = self.segments.pop_front() {
                self.dropped_discontinuities += u64::from(s.discontinuity);
            }
        }
    }
}
