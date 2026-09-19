//! WebVTT subtitle segments for the captions rendition (RFC 8216bis
//! §3.1.4): one `.vtt` file per media segment, same sequence number and
//! duration, holding every cue that overlaps it (a cue spanning two
//! segments is written in both; players drop the duplicate).
//!
//! Cue times are "local" WebVTT times equal to the stream's media time
//! (the `Cue::at_us` clock). `X-TIMESTAMP-MAP` ties local 0 to media time 0
//! on the fMP4 timeline, which runs [`SHIFT_SECS`](crate::packager::SHIFT_SECS)
//! ahead of media time, in 90 kHz units (33-bit, as in MPEG-TS).

use std::fmt::Write as _;

use caudal_core::captions::TextCue;

use crate::packager::SHIFT_SECS;

const WRAP: i64 = 1 << 33;

/// `HH:MM:SS.mmm` (hours unbounded, as WebVTT allows). Negative times
/// clamp to zero.
pub(crate) fn timestamp(us: i64) -> String {
    let ms = us.max(0) / 1000;
    format!("{:02}:{:02}:{:02}.{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

/// The `MPEGTS` value (90 kHz, 33-bit) of media time `us` on the fMP4
/// timeline.
pub(crate) fn mpegts(us: i64) -> i64 {
    let fmp4_us = us + SHIFT_SECS * 1_000_000;
    (i128::from(fmp4_us) * 90_000 / 1_000_000).rem_euclid(i128::from(WRAP)) as i64
}

/// One subtitle segment covering `[start_us, end_us)` of media time.
pub(crate) fn segment(cues: &[TextCue], start_us: i64, end_us: i64) -> String {
    // The same map in every file (local 0 = media time 0): a cue repeated
    // in two files then gets bit-identical times in the player, which drops
    // the duplicate. (With a per-file LOCAL, hls.js's float arithmetic put
    // the copies 1-10 ms apart and showed both: browser check, 19 Sep 2026.)
    let mut o = String::from("WEBVTT\n");
    let _ = writeln!(o, "X-TIMESTAMP-MAP=MPEGTS:{},LOCAL:{}", mpegts(0), timestamp(0));
    for c in cues.iter().filter(|c| c.start_us < end_us && c.end_us > start_us) {
        let text = sanitize(&c.text);
        if text.is_empty() {
            continue;
        }
        let _ = write!(o, "\n{} --> {}\n{text}\n", timestamp(c.start_us), timestamp(c.end_us.max(c.start_us + 1000)));
    }
    o
}

/// Cue text is plain text: escape what WebVTT would parse as markup, and
/// never let a blank line (the end of a cue) or `-->` through.
fn sanitize(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| l.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cue(start_us: i64, end_us: i64, text: &str) -> TextCue {
        TextCue { start_us, end_us, text: text.to_owned() }
    }

    #[test]
    fn timestamps_are_webvtt() {
        assert_eq!(timestamp(0), "00:00:00.000");
        assert_eq!(timestamp(61_234_567), "00:01:01.234");
        assert_eq!(timestamp(3_600_000_000 * 27 + 5_000), "27:00:00.005");
        assert_eq!(timestamp(-5), "00:00:00.000");
    }

    #[test]
    fn timestamp_map_follows_the_fmp4_shift_and_wraps_at_33_bits() {
        // Media time 0 is fMP4 time 10 s: 900000 ticks at 90 kHz.
        assert_eq!(mpegts(0), 900_000);
        assert_eq!(mpegts(2_000_000), 1_080_000);
        // Past 2^33 ticks (~26.5 h) the value wraps like an MPEG-TS PTS.
        let wrap_us = WRAP * 1_000_000 / 90_000 - SHIFT_SECS * 1_000_000;
        assert!(mpegts(wrap_us + 1_000_000) < 100_000, "{}", mpegts(wrap_us + 1_000_000));
        // Every local time t maps to media time t: LOCAL and MPEGTS move
        // together, 90 ticks per ms.
        assert_eq!(mpegts(12_345_000) - mpegts(12_000_000), 345 * 90);
    }

    #[test]
    fn a_segment_holds_the_cues_that_overlap_it() {
        let cues = [
            cue(1_000_000, 3_000_000, "antes"),
            cue(9_000_000, 12_500_000, "Buenas tardes y\nbienvenidos"),
            cue(12_500_000, 16_000_000, "a la transmisión"),
            cue(20_000_000, 21_000_000, "después"),
        ];
        let v = segment(&cues, 10_000_000, 14_000_000);
        assert_eq!(
            v,
            "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:900000,LOCAL:00:00:00.000\n\
             \n00:00:09.000 --> 00:00:12.500\nBuenas tardes y\nbienvenidos\n\
             \n00:00:12.500 --> 00:00:16.000\na la transmisión\n"
        );
        // Cue boundaries are exclusive: a cue ending where the segment
        // starts is not in it.
        assert!(!segment(&cues, 3_000_000, 4_000_000).contains("antes"));
        // Nothing to say is still a valid segment.
        assert_eq!(segment(&[], 0, 2_000_000), "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:900000,LOCAL:00:00:00.000\n");
    }

    #[test]
    fn text_cannot_break_out_of_its_cue() {
        let v = segment(&[cue(0, 1_000_000, "a <b>x</b> & y\n\n--> 00:00:00.000\nz")], 0, 2_000_000);
        assert!(v.contains("a &lt;b&gt;x&lt;/b&gt; &amp; y\n--&gt; 00:00:00.000\nz\n"), "{v}");
        assert_eq!(v.matches("-->").count(), 1);
    }
}
