//! Live MPEG-DASH over the LL-HLS packager's segments.
//!
//! `GET /hls/{name}/manifest.mpd` is a dynamic MPD whose `SegmentTimeline`
//! lists the same complete CMAF segments `index.m3u8` lists, with their
//! exact durations. Those segments carry video and audio muxed in one
//! fragment, while DASH players want one media type per `AdaptationSet`
//! (dash.js refuses multiplexed Representations; ExoPlayer only exposes the
//! track type its `AdaptationSet` declares). So each segment is served two
//! more ways, cut out of the stored bytes per request rather than stored
//! again: `v{msn}.m4s` (video only) and `a{msn}.m4s` (audio only), each
//! decoded by its own one-track init segment `v-init{gen}.mp4` /
//! `a-init{gen}.mp4`.
//!
//! Every discontinuity (a republish within the reconnect grace, a new init
//! segment, a timestamp jump) opens a new `Period`; see
//! [`crate::packager::PeriodMark`].
//!
//! `GET /hls/{name}/ll.mpd` is the low-latency variant (DASH-IF
//! "Low-latency Modes for DASH", the IOP v4.3 change request, clause
//! 4.X.3.2): the same timeline plus the segment being built, announced as
//! soon as its first part (CMAF chunk) exists, with
//! `SegmentTemplate@availabilityTimeOffset` = segment duration - part
//! duration and `@availabilityTimeComplete="false"`, a `ServiceDescription`
//! (target/min/max latency, playback-rate bounds) and a
//! `ProducerReferenceTime` per Representation. A request for that open
//! segment is answered at once and streamed part by part (HTTP chunked
//! transfer) until the segment closes; see `dash_file` in `lib.rs`.

use std::fmt::Write as _;
use std::time::SystemTime;

use bytes::Bytes;

use crate::LlDash;
use crate::packager::{DashTrack, Packager, PeriodMark, TrackRun, rfc3339};

pub(crate) const CONTENT_TYPE: &str = "application/dash+xml";

/// One complete segment as the MPD describes it.
#[derive(Clone)]
pub(crate) struct Seg {
    pub msn: u64,
    pub period: PeriodMark,
    /// Media time of its first frame (`Cue::at_us` clock), to line
    /// renditions up.
    pub start_us: i64,
    pub init_gen: u32,
    pub runs: [Option<TrackRun>; 2],
    /// The tracks of its init generation, primary first.
    pub tracks: Vec<DashTrack>,
    /// Still being built (only in an LL snapshot): `runs` cover the parts
    /// so far, and the timeline gives it its nominal duration.
    pub open: bool,
}

/// What an LL snapshot adds: the packager's part and segment lengths and
/// the service description.
#[derive(Clone, Copy)]
pub(crate) struct Ll {
    pub segment_ms: u32,
    pub part_ms: u32,
    pub service: LlDash,
}

impl Ll {
    /// `availabilityTimeOffset`, seconds: a segment may be requested once
    /// its first chunk is out, i.e. a segment duration minus a part before
    /// it completes (clause 4.X.3.2, 2.e: at most the segment duration
    /// minus its first chunk's; our first part is never longer than
    /// `part_ms`, and segments never shorter than `segment_ms`).
    pub fn ato(&self) -> f64 {
        f64::from(self.segment_ms - self.part_ms) / 1000.0
    }
}

/// Everything one rendition contributes to an MPD, copied out from under
/// its packager lock.
pub(crate) struct Snap {
    pub epoch: SystemTime,
    pub ended: bool,
    pub target: u64,
    pub segs: Vec<Seg>,
    /// `Some` for the low-latency manifest.
    pub ll: Option<Ll>,
}

impl Snap {
    /// The plain manifest's view: complete segments only. `None` until
    /// the rendition has a complete segment.
    pub fn of(p: &Packager) -> Option<Self> {
        Self::build(p, false)
    }

    /// The LL manifest's view: also the open segment, once it has a part.
    /// Plain when parts are as long as segments (no chunk to send early).
    pub fn ll(p: &Packager) -> Option<Self> {
        Self::build(p, true)
    }

    fn build(p: &Packager, ll: bool) -> Option<Self> {
        let cfg = p.cfg();
        let ll = (ll && cfg.segment_ms > cfg.part_ms).then(|| Ll {
            segment_ms: cfg.segment_ms,
            part_ms: cfg.part_ms,
            service: cfg.ll_dash.resolve(cfg.segment_ms),
        });
        let open_ok = ll.is_some() && !p.ended;
        let segs: Vec<Seg> = p
            .segments
            .iter()
            .filter(|s| s.full.is_some() || (open_ok && !s.parts.is_empty()))
            .filter_map(|s| {
                Some(Seg {
                    msn: s.msn,
                    period: s.period,
                    start_us: s.start_us,
                    init_gen: s.init_gen,
                    runs: [s.run(0), s.run(1)],
                    tracks: p.dash_tracks(s.init_gen)?.to_vec(),
                    open: s.full.is_none(),
                })
            })
            .collect();
        if segs.iter().all(|s| s.open) {
            return None;
        }
        Some(Self { epoch: p.epoch?, ended: p.ended, target: p.target_duration(), segs, ll })
    }
}

/// A file a DASH Representation names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum File {
    /// `v-init{gen}.mp4` / `a-init{gen}.mp4`.
    Init { video: bool, generation: u32 },
    /// `v{msn}.m4s` / `a{msn}.m4s`.
    Media { video: bool, msn: u64 },
}

pub(crate) fn parse_name(file: &str) -> Option<File> {
    let (video, rest) = match file.as_bytes().first()? {
        b'v' => (true, &file[1..]),
        b'a' => (false, &file[1..]),
        _ => return None,
    };
    fn number<T: std::str::FromStr>(s: &str) -> Option<T> {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) || (s.len() > 1 && s.starts_with('0')) {
            return None;
        }
        s.parse().ok()
    }
    if let Some(g) = rest.strip_prefix("-init").and_then(|r| r.strip_suffix(".mp4")) {
        return Some(File::Init { video, generation: number(g)? });
    }
    Some(File::Media { video, msn: number(rest.strip_suffix(".m4s")?)? })
}

fn letter(video: bool) -> char {
    if video { 'v' } else { 'a' }
}

/// The one-track init segment, or the one-track view of a complete
/// segment, from a packager. Media lookups are done by the caller (which
/// may have to wait); this only cuts.
pub(crate) fn init(p: &Packager, video: bool, generation: u32) -> Option<Bytes> {
    p.dash_tracks(generation)?.iter().find(|t| t.video == video).map(|t| t.init.clone())
}

/// The track id a complete segment's `video`/`audio` run has.
pub(crate) fn track_id(p: &Packager, msn: u64, video: bool) -> Option<u32> {
    let seg = p.segment_at(msn)?;
    p.dash_tracks(seg.init_gen)?.iter().find(|t| t.video == video).map(|t| t.track_id)
}

/// `ticks` on clock `from` → ticks on clock `to`, rounded to nearest.
fn rescale(ticks: i64, from: u32, to: u32) -> i64 {
    if from == to {
        return ticks;
    }
    let (n, d) = (i128::from(ticks) * i128::from(to), i128::from(from.max(1)));
    ((n + d / 2).div_euclid(d)) as i64
}

/// `PT1.234S`.
fn duration(us: i64) -> String {
    format!("PT{:.3}S", us.max(0) as f64 / 1e6)
}

fn xml_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            c => o.push(c),
        }
    }
    o
}

/// `frameRate`: an integer, or `n/1001` for the NTSC rates.
fn frame_rate(fps: f64) -> String {
    if (fps - fps.round()).abs() < 0.001 {
        return format!("{}", fps.round() as u64);
    }
    let ntsc = (fps * 1001.0).round();
    if (ntsc / 1001.0 - fps).abs() < 0.001 {
        return format!("{}/1001", ntsc as u64);
    }
    format!("{}/1000", (fps * 1000.0).round() as u64)
}

/// One `Representation`: the rendition named `name`, track `lane` of the
/// period whose segments are `segs` (all of one init generation).
struct Rep<'a> {
    name: &'a str,
    id: String,
    track: &'a DashTrack,
    lane: usize,
    segs: Vec<&'a Seg>,
    /// `presentationTimeOffset`, on `track.timescale`.
    pto: i64,
    /// LL only: the period's `ProducerReferenceTime@wallClockTime`.
    prt_wall: Option<String>,
}

impl Rep<'_> {
    fn bandwidth(&self) -> u64 {
        let ts = f64::from(self.track.timescale.max(1));
        self.segs
            .iter()
            .filter(|s| !s.open)
            .filter_map(|s| s.runs[self.lane])
            .filter(|r| r.dur > 0)
            .map(|r| (r.bytes as f64 * 8.0 / (r.dur as f64 / ts)) as u64)
            .max()
            .unwrap_or(0)
            .max(1000)
    }

    /// `utc`: the `UTCTiming` element a `ProducerReferenceTime` repeats.
    fn write(&self, o: &mut String, token: Option<&str>, ll: Option<&Ll>, utc: &str) {
        let t = self.track;
        let _ = write!(
            o,
            "      <Representation id=\"{}\" codecs=\"{}\" bandwidth=\"{}\"",
            xml_escape(&self.id),
            t.codec,
            self.bandwidth()
        );
        if let Some((w, h)) = t.resolution {
            let _ = write!(o, " width=\"{w}\" height=\"{h}\"");
        }
        if let Some(fps) = t.frame_rate {
            let _ = write!(o, " frameRate=\"{}\"", frame_rate(fps));
        }
        if let Some(rate) = t.sample_rate.filter(|_| !t.video) {
            let _ = write!(o, " audioSamplingRate=\"{rate}\"");
        }
        o.push_str(">\n");
        if let Some(ch) = t.channels.filter(|_| !t.video) {
            let _ = writeln!(
                o,
                "        <AudioChannelConfiguration schemeIdUri=\"urn:mpeg:dash:23003:3:audio_channel_configuration:2011\" value=\"{ch}\"/>"
            );
        }
        if let (Some(_), Some(wall)) = (ll, &self.prt_wall) {
            // Clause 4.X.3.2, 1: the wall clock at `presentationTime` =
            // the `presentationTimeOffset` (the period start), with a
            // `UTCTiming` identical to one of the MPD's.
            let _ = writeln!(
                o,
                "        <ProducerReferenceTime id=\"0\" type=\"encoder\" wallClockTime=\"{wall}\" presentationTime=\"{}\">\n          {utc}\n        </ProducerReferenceTime>",
                self.pto
            );
        }
        let q = token.map(|t| format!("?token={t}")).unwrap_or_default();
        let name = xml_escape(self.name);
        let l = letter(t.video);
        let first = self.segs.first().map_or(0, |s| s.msn);
        let generation = self.segs.first().map_or(0, |s| s.init_gen);
        let _ = writeln!(
            o,
            "        <SegmentTemplate timescale=\"{}\" presentationTimeOffset=\"{}\" startNumber=\"{first}\" initialization=\"../{name}/{l}-init{generation}.mp4{q}\" media=\"../{name}/{l}$Number$.m4s{q}\"{}>",
            t.timescale,
            self.pto,
            ll.map(|ll| format!(" availabilityTimeOffset=\"{:.3}\" availabilityTimeComplete=\"false\"", ll.ato()))
                .unwrap_or_default()
        );
        // The open segment's nominal length: it is cut on the first
        // keyframe after `segment_ms`, so it may end up longer; the next
        // update lists its real `d`.
        let nominal = ll.map_or(0, |ll| (u64::from(ll.segment_ms) * u64::from(t.timescale.max(1))).div_ceil(1000));
        o.push_str("          <SegmentTimeline>\n");
        // Every entry carries its `t`: the timeline stays exact across the
        // small gaps a track can have between segments (audio is cut on
        // frame boundaries, not on the video's).
        let mut next: Option<i64> = None;
        for s in &self.segs {
            let (start, dur) = match s.runs[self.lane] {
                Some(r) => (r.start, r.dur),
                // No samples of this track in the segment (rare: audio
                // that stopped): keep the numbering, fill with the primary
                // track's span. Its request answers 404.
                None => {
                    let p = s.runs[0].unwrap_or(TrackRun { start: 0, dur: 0, bytes: 0 });
                    let ts0 = s.tracks.first().map_or(1, |t| t.timescale);
                    let start = next.unwrap_or_else(|| rescale(p.start, ts0, t.timescale));
                    (start, rescale(p.dur as i64, ts0, t.timescale).max(1) as u64)
                }
            };
            let dur = if s.open { dur.max(nominal) } else { dur };
            let _ = writeln!(o, "            <S t=\"{start}\" d=\"{dur}\"/>");
            next = Some(start + dur as i64);
        }
        o.push_str("          </SegmentTimeline>\n        </SegmentTemplate>\n      </Representation>\n");
    }
}

/// Everything a manifest request needs besides the snapshots.
pub(crate) struct Ctx<'a> {
    pub token: Option<&'a str>,
    /// Absolute URL of `/dash/time`, when the request said which host it
    /// was for.
    pub time_url: Option<String>,
    pub now: SystemTime,
}

/// The MPD for rendition `lead` plus its family `siblings` (other
/// `{root}+*` renditions), which join as further video Representations of
/// the current period. Renditions must share the lead's timestamps (as
/// `caudal-transcode`'s do); each keeps its own segment numbers and
/// timeline.
pub(crate) fn render(lead: (&str, &Snap), siblings: &[(String, Snap)], ctx: &Ctx<'_>) -> String {
    let (lead_name, snap) = lead;
    let target_us = snap.target as i64 * 1_000_000;
    let window_us: i64 = snap
        .segs
        .iter()
        .filter(|s| !s.open)
        .filter_map(|s| {
            let r = s.runs[0]?;
            let ts = i128::from(s.tracks.first()?.timescale.max(1));
            Some((i128::from(r.dur) * 1_000_000 / ts) as i64)
        })
        .sum();
    let mut o = String::with_capacity(4096);
    o.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    let _ = write!(
        o,
        "<MPD xmlns=\"urn:mpeg:dash:schema:mpd:2011\" profiles=\"urn:mpeg:dash:profile:isoff-live:2011\" type=\"dynamic\" availabilityStartTime=\"{}\" publishTime=\"{}\"",
        rfc3339(snap.epoch),
        rfc3339(ctx.now)
    );
    let ll = snap.ll.as_ref();
    if !snap.ended {
        // LL: the open segment appears once its first part exists, so the
        // player has to look again well within a segment.
        let every = ll.map_or(target_us, |ll| i64::from(ll.part_ms.max(500)) * 1000);
        let _ = write!(o, " minimumUpdatePeriod=\"{}\"", duration(every));
    }
    // `suggestedPresentationDelay` stays the plain one (3 target
    // durations, like HLS `HOLD-BACK`) for players that ignore the LL
    // signalling and wait for complete segments; LL players aim for
    // `ServiceDescription/Latency@target` instead.
    let delay = 3 * target_us;
    let _ = writeln!(
        o,
        " timeShiftBufferDepth=\"{}\" suggestedPresentationDelay=\"{}\" minBufferTime=\"{}\">",
        duration(window_us.max(target_us)),
        duration(delay),
        duration(ll.map_or(target_us, |ll| i64::from(ll.part_ms) * 1000).min(target_us))
    );
    if let Some(ll) = ll {
        // Clause 4.X.3.2: a `ServiceDescription` with `Latency` (against
        // `ProducerReferenceTime` 0) and `PlaybackRate` bounds; element
        // names as in ISO/IEC 23009-1:2022 Annex K.
        let sd = ll.service;
        let _ = writeln!(
            o,
            "  <ServiceDescription id=\"0\">\n    <Latency referenceId=\"0\" target=\"{}\" min=\"{}\" max=\"{}\"/>\n    <PlaybackRate min=\"{}\" max=\"{}\"/>\n  </ServiceDescription>",
            sd.target_latency_ms, sd.min_latency_ms, sd.max_latency_ms, sd.min_playback_rate, sd.max_playback_rate
        );
    }
    let utc_http = ctx.time_url.as_ref().map(|url| {
        format!("<UTCTiming schemeIdUri=\"urn:mpeg:dash:utc:http-iso:2014\" value=\"{}\"/>", xml_escape(url))
    });
    let utc_direct =
        format!("<UTCTiming schemeIdUri=\"urn:mpeg:dash:utc:direct:2014\" value=\"{}\"/>", rfc3339(ctx.now));
    let utc_prt = utc_http.as_deref().unwrap_or(&utc_direct);

    // Periods in order of their first segment.
    let mut periods: Vec<(PeriodMark, Vec<&Seg>)> = Vec::new();
    for s in &snap.segs {
        match periods.last_mut() {
            Some((p, segs)) if p.msn == s.period.msn => segs.push(s),
            _ => periods.push((s.period, vec![s])),
        }
    }
    let last = periods.len().saturating_sub(1);
    for (i, (mark, segs)) in periods.iter().enumerate() {
        let _ = writeln!(o, "  <Period id=\"p{}\" start=\"{}\">", mark.msn, duration(mark.start_us));
        let tracks = &segs[0].tracks;
        let prt_wall = ll.map(|_| rfc3339(snap.epoch + std::time::Duration::from_micros(mark.start_us.max(0) as u64)));
        for (lane, track) in tracks.iter().enumerate().take(2) {
            let mut reps = vec![Rep {
                name: lead_name,
                id: if track.video { lead_name.to_owned() } else { format!("{lead_name}-audio") },
                track,
                lane,
                segs: segs.clone(),
                pto: rescale(mark.anchor, mark.timescale, track.timescale),
                prt_wall: prt_wall.clone(),
            }];
            if track.video && i == last {
                reps.extend(sibling_reps(siblings, mark, segs[0].start_us, &prt_wall));
            }
            let kind = if track.video { "video" } else { "audio" };
            let _ = write!(
                o,
                "    <AdaptationSet id=\"{lane}\" contentType=\"{kind}\" mimeType=\"{kind}/mp4\" startWithSAP=\"1\""
            );
            if reps.len() == 1 {
                o.push_str(" segmentAlignment=\"true\"");
            }
            if let Some(lang) = &track.lang {
                let _ = write!(o, " lang=\"{}\"", xml_escape(lang));
            }
            o.push_str(">\n");
            reps.sort_by_key(|r| std::cmp::Reverse(r.bandwidth()));
            for r in &reps {
                r.write(&mut o, ctx.token, ll, utc_prt);
            }
            o.push_str("    </AdaptationSet>\n");
        }
        o.push_str("  </Period>\n");
    }
    if let Some(u) = &utc_http {
        let _ = writeln!(o, "  {u}");
    }
    let _ = writeln!(o, "  {utc_direct}");
    o.push_str("</MPD>\n");
    o
}

/// The video Representations other family renditions add to the lead's
/// current period: each sibling's segments from its own current period
/// that start no earlier than the lead period's first listed segment.
fn sibling_reps<'a>(
    siblings: &'a [(String, Snap)],
    mark: &PeriodMark,
    lead_start_us: i64,
    prt_wall: &Option<String>,
) -> Vec<Rep<'a>> {
    let mut out = Vec::new();
    for (name, snap) in siblings {
        let Some(cur) = snap.segs.last().map(|s| s.period.msn) else { continue };
        let segs: Vec<&Seg> =
            snap.segs.iter().filter(|s| s.period.msn == cur && s.start_us + 100_000 >= lead_start_us).collect();
        let Some(&first) = segs.first() else { continue };
        let Some(lane) = first.tracks.iter().position(|t| t.video) else { continue };
        let track = &first.tracks[lane];
        // One init generation per Representation.
        let segs: Vec<&Seg> = segs.into_iter().take_while(|s| s.init_gen == first.init_gen).collect();
        out.push(Rep {
            name,
            id: name.clone(),
            track,
            lane,
            pto: rescale(mark.anchor, mark.timescale, track.timescale),
            prt_wall: prt_wall.clone(),
            segs,
        });
    }
    out
}

/// The `/dash/time` URL for a request with these `Host` and
/// `X-Forwarded-Proto` values; `None` when the host is missing or holds
/// anything a host name cannot.
pub(crate) fn time_url(host: Option<&str>, proto: Option<&str>) -> Option<String> {
    let host = host?;
    let ok = !host.is_empty()
        && host.len() <= 255
        && host.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'));
    if !ok {
        return None;
    }
    let scheme = match proto.map(|p| p.split(',').next().unwrap_or("").trim()) {
        Some(p) if p.eq_ignore_ascii_case("https") => "https",
        _ => "http",
    };
    Some(format!("{scheme}://{host}/dash/time"))
}
