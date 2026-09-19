//! One output timeline across source switches.
//!
//! Every source has its own clock: an encoder that starts at zero, a camera
//! that has been up for days, a file that loops. The output must never go
//! back in time, or players and packagers see a jump. Like
//! `caudal-channel` stitching files, each switch starts a *segment*: the
//! new source's first join point (a video keyframe, or any frame of an
//! audio-only source) is placed where the output's video left off (last
//! dts plus last frame duration; all tracks when there is no video), and
//! every frame of the segment is shifted by the same amount.
//!
//! Video is the timeline: it continues frame for frame, so the part and
//! segment around a switch keep their normal durations (a gap there would
//! make an LL-HLS part longer than its target). Audio often runs a few
//! hundred milliseconds ahead of video in a live feed; new audio that
//! would land on or before audio already sent is dropped rather than
//! squeezed, and a short gap is left when the old audio ended early.
//! Video that would land on or before the previous frame (a rounding, a
//! source hiccup) is bumped to `last dts + 1`, keeping it strictly
//! monotonic.

use std::collections::HashMap;

use caudal_core::{Frame, TrackId, TrackInfo, TrackKind};

#[derive(Debug, Clone, Copy)]
struct Last {
    video: bool,
    dts: i64,
    /// Last frame duration seen on this track, on its clock.
    delta: i64,
    timescale: u32,
}

#[derive(Debug, Clone, Copy)]
struct Segment {
    /// Source time (µs) of the segment's join point.
    t0: i64,
    /// Output time = source time + `shift` (µs).
    shift: i64,
    /// The join track and its shift in its own ticks, exact (no rounding
    /// through microseconds), so video continues frame for frame.
    exact: (TrackId, i64),
}

#[derive(Default)]
pub(crate) struct Stitcher {
    tracks: Vec<TrackInfo>,
    last: HashMap<TrackId, Last>,
    seg: Option<Segment>,
}

/// `v * num / den`, rounded up, without overflow.
fn scale_ceil(v: i64, num: u32, den: u32) -> i64 {
    let (n, d) = (i128::from(v) * i128::from(num.max(1)), i128::from(den.max(1)));
    (n.div_euclid(d) + i128::from(n.rem_euclid(d) != 0)) as i64
}

impl Stitcher {
    /// The source's track list (on attach, and whenever it changes).
    pub(crate) fn set_tracks(&mut self, tracks: Vec<TrackInfo>) {
        self.tracks = tracks;
    }

    /// Starts a new segment: frames are dropped until the next join point.
    pub(crate) fn begin(&mut self) {
        self.seg = None;
    }

    /// Where the output left off, in microseconds: the latest video end,
    /// or the latest end over all tracks without video. `None` before any
    /// frame went out.
    fn end_micros(&self) -> Option<i64> {
        let end = |l: &Last| scale_ceil(l.dts + l.delta, 1_000_000, l.timescale);
        self.last.values().filter(|l| l.video).map(end).max().or_else(|| self.last.values().map(end).max())
    }

    /// Restamps `f` onto the output timeline, or drops it (`None`): before
    /// the segment's join point, or an unannounced track.
    pub(crate) fn stamp(&mut self, mut f: Frame) -> Option<Frame> {
        let info = self.tracks.iter().find(|t| t.id == f.track)?;
        let video = info.kind() == TrackKind::Video;
        let ts = info.timescale.max(1);
        let src = info.to_micros(f.dts);
        let seg = match self.seg {
            Some(s) => s,
            None => {
                let has_video = self.tracks.iter().any(|t| t.kind() == TrackKind::Video);
                if has_video && !(video && f.keyframe) {
                    return None;
                }
                let base = self.end_micros().unwrap_or(src);
                let s = match self.last.get(&f.track) {
                    // Continue this track exactly where it ended.
                    Some(l) if l.timescale == ts && scale_ceil(l.dts + l.delta, 1_000_000, ts) == base => {
                        Segment { t0: src, shift: base - src, exact: (f.track, l.dts + l.delta - f.dts) }
                    }
                    _ => {
                        Segment { t0: src, shift: base - src, exact: (f.track, scale_ceil(base - src, ts, 1_000_000)) }
                    }
                };
                self.seg = Some(s);
                s
            }
        };
        // Audio that was interleaved just before the keyframe belongs to
        // the previous source's time range.
        if !video && src < seg.t0 {
            return None;
        }
        let shift = if seg.exact.0 == f.track { seg.exact.1 } else { scale_ceil(seg.shift, ts, 1_000_000) };
        f.dts += shift;
        f.pts += shift;
        match self.last.get(&f.track).copied() {
            Some(l) => {
                let prev = if l.timescale == ts { l.dts } else { scale_ceil(l.dts, ts, l.timescale) };
                if f.dts <= prev && !video {
                    return None;
                }
                if f.dts <= prev {
                    let bump = prev + 1 - f.dts;
                    f.dts += bump;
                    f.pts += bump;
                }
                let delta = if l.timescale == ts { f.dts - prev } else { 0 };
                let delta = if delta > 0 { delta } else { l.delta };
                self.last.insert(f.track, Last { video, dts: f.dts, delta, timescale: ts });
            }
            None => {
                self.last.insert(f.track, Last { video, dts: f.dts, delta: 0, timescale: ts });
            }
        }
        Some(f)
    }

    /// Maps a cue time (µs, source clock) onto the output clock. `None`
    /// before the segment's join point.
    pub(crate) fn cue_time(&self, at_us: i64) -> Option<i64> {
        self.seg.map(|s| at_us + s.shift)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use caudal_core::{AudioParams, Codec, VideoParams};

    pub(crate) fn video(id: u32) -> TrackInfo {
        TrackInfo {
            id: TrackId(id),
            codec: Codec::H264,
            timescale: 90_000,
            init: Bytes::from_static(b"avcC"),
            lang: None,
            video: Some(VideoParams { width: 1280, height: 720, fps: None }),
            audio: None,
        }
    }

    pub(crate) fn audio(id: u32) -> TrackInfo {
        TrackInfo {
            id: TrackId(id),
            codec: Codec::Aac,
            timescale: 48_000,
            init: Bytes::from_static(&[0x11, 0x90]),
            lang: None,
            video: None,
            audio: Some(AudioParams { sample_rate: 48_000, channels: 2 }),
        }
    }

    fn frame(track: u32, dts: i64, keyframe: bool) -> Frame {
        Frame { track: TrackId(track), dts, pts: dts, keyframe, data: Bytes::from_static(b"x") }
    }

    /// 30 fps video (3000 ticks) and AAC (1024 samples), from `v0`/`a0`.
    fn feed(s: &mut Stitcher, v0: i64, a0: i64, n: i64, out: &mut Vec<Frame>) {
        for i in 0..n {
            out.extend(s.stamp(frame(0, v0 + i * 3000, i % 30 == 0)));
            out.extend(s.stamp(frame(1, a0 + i * 1024, true)));
        }
    }

    fn assert_monotonic(frames: &[Frame]) {
        let mut last: HashMap<TrackId, i64> = HashMap::new();
        for f in frames {
            if let Some(&p) = last.get(&f.track) {
                assert!(f.dts > p, "track {:?} went {p} -> {}", f.track, f.dts);
            }
            last.insert(f.track, f.dts);
        }
    }

    #[test]
    fn timestamps_stay_monotonic_across_switches() {
        let mut s = Stitcher::default();
        let mut out = Vec::new();
        s.set_tracks(vec![video(0), audio(1)]);
        s.begin();
        // A primary far into its own clock.
        feed(&mut s, 900_000_000, 480_000_000, 90, &mut out);
        let end_primary = out.iter().filter(|f| f.track == TrackId(0)).map(|f| f.dts).max().unwrap();
        // A backup whose clock starts at zero.
        s.begin();
        let before = out.len();
        feed(&mut s, 0, 0, 90, &mut out);
        let first_backup = &out[before];
        assert_eq!(first_backup.track, TrackId(0));
        assert!(first_backup.keyframe);
        assert_eq!(first_backup.dts, end_primary + 3000, "continues one frame after the primary");
        // And back to the primary, whose clock kept running meanwhile.
        s.begin();
        feed(&mut s, 900_000_000 + 180 * 3000, 480_000_000 + 180 * 1024, 90, &mut out);
        assert_monotonic(&out);
    }

    #[test]
    fn video_continues_frame_for_frame_when_audio_ran_ahead() {
        let mut s = Stitcher::default();
        s.set_tracks(vec![video(0), audio(1)]);
        s.begin();
        let mut out = Vec::new();
        feed(&mut s, 0, 0, 30, &mut out);
        // The old source's audio ran 300 ms (14 frames) past its video.
        for i in 30..44 {
            out.extend(s.stamp(frame(1, i * 1024, true)));
        }
        let last_video = out.iter().rev().find(|f| f.track == TrackId(0)).unwrap().dts;
        let last_audio = out.iter().rev().find(|f| f.track == TrackId(1)).unwrap().dts;
        s.begin();
        let before = out.len();
        feed(&mut s, 5_000_000, 5_000_000 * 48 / 90, 30, &mut out);
        let new = &out[before..];
        assert_eq!(new[0].dts, last_video + 3000, "no gap in video");
        let first_audio = new.iter().find(|f| f.track == TrackId(1)).unwrap();
        assert!(first_audio.dts > last_audio, "overlapping audio is dropped, not squeezed");
        assert_monotonic(&out);
        // Dropped rather than bumped: consecutive new audio keeps its spacing.
        let audio: Vec<i64> = new.iter().filter(|f| f.track == TrackId(1)).map(|f| f.dts).collect();
        assert!(audio.windows(2).all(|w| w[1] - w[0] == 1024), "{audio:?}");
    }

    #[test]
    fn a_segment_waits_for_a_keyframe_and_drops_earlier_audio() {
        let mut s = Stitcher::default();
        s.set_tracks(vec![video(0), audio(1)]);
        s.begin();
        assert!(s.stamp(frame(1, 0, true)).is_none(), "audio before any keyframe");
        assert!(s.stamp(frame(0, 3000, false)).is_none(), "a delta frame cannot start a segment");
        assert!(s.seg.is_none());
        let k = s.stamp(frame(0, 6000, true)).expect("keyframe starts it");
        assert!(s.seg.is_some());
        assert_eq!(k.dts, 6000, "the first segment keeps the source clock");
        // AAC at 48 kHz: 3000 samples = 62.5 ms, before the keyframe's 66.7 ms.
        assert!(s.stamp(frame(1, 3000, true)).is_none(), "audio before the join point");
        assert!(s.stamp(frame(1, 3300, true)).is_some());
    }

    #[test]
    fn audio_only_sources_join_on_any_frame() {
        let mut s = Stitcher::default();
        s.set_tracks(vec![audio(0)]);
        s.begin();
        assert_eq!(s.stamp(frame(0, 480, true)).unwrap().dts, 480);
    }

    #[test]
    fn a_timescale_change_keeps_the_track_monotonic() {
        let mut s = Stitcher::default();
        s.set_tracks(vec![video(0)]);
        s.begin();
        let a = s.stamp(frame(0, 90_000, true)).unwrap();
        assert_eq!(a.dts, 90_000);
        let mut v = video(0);
        v.timescale = 1000;
        s.set_tracks(vec![v]);
        s.begin();
        let b = s.stamp(frame(0, 5, true)).unwrap();
        assert!(b.dts > 1000, "1 s on the new clock or later, got {}", b.dts);
    }

    #[test]
    fn cues_follow_the_segment_shift() {
        let mut s = Stitcher::default();
        s.set_tracks(vec![video(0)]);
        s.begin();
        assert_eq!(s.cue_time(5), None);
        s.stamp(frame(0, 90_000, true));
        s.stamp(frame(0, 93_000, false));
        s.begin();
        s.stamp(frame(0, 0, true));
        // Output continued at 96 000 ticks = 1 066 667 µs (rounded up).
        assert_eq!(s.cue_time(0), Some(1_066_667));
    }
}
