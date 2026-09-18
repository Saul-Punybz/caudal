//! `meta.json`, the playlist text, file names and the date formats they use.

use std::fmt::Write as _;
use std::path::Path;
use std::time::{Duration, SystemTime};

use caudal_cmaf::fmp4::Mp4Track;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct Meta {
    pub stream: String,
    pub id: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub duration_ms: u64,
    pub bytes: u64,
    pub segments: u32,
    pub tracks: Vec<TrackMeta>,
    /// Why the recording stopped early (disk full, crash), if it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct TrackMeta {
    pub kind: String,
    pub codec: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub sample_rate: Option<u32>,
}

impl TrackMeta {
    pub fn from_track(t: &Mp4Track) -> Self {
        let i = &t.info;
        Self {
            kind: i.kind().as_str().into(),
            codec: i.codec.as_str().into(),
            width: i.video.map(|v| v.width),
            height: i.video.map(|v| v.height),
            sample_rate: i.audio.map(|a| a.sample_rate).or((i.video.is_none()).then_some(i.timescale)),
        }
    }
}

/// One closed segment as listed in the playlist.
#[derive(Debug, Clone)]
pub(crate) struct SegEntry {
    pub index: u32,
    pub duration: f64,
    pub discontinuity: bool,
    /// Written on the first segment and after every discontinuity.
    pub pdt: Option<SystemTime>,
}

pub(crate) fn segment_name(index: u32) -> String {
    format!("seg-{index:06}.m4s")
}

/// `seg-NNNNNN.m4s` → N.
pub(crate) fn parse_segment_name(name: &str) -> Option<u32> {
    let n = name.strip_prefix("seg-")?.strip_suffix(".m4s")?;
    (n.len() == 6 && n.bytes().all(|b| b.is_ascii_digit())).then(|| n.parse().ok())?
}

/// `YYYYMMDDTHHMMSSZ`, optionally followed by `-N` for same-second restarts.
pub(crate) fn valid_id(id: &str) -> bool {
    let (base, suffix) = match id.split_once('-') {
        Some((b, s)) => (b, Some(s)),
        None => (id, None),
    };
    let b = base.as_bytes();
    let base_ok = b.len() == 16
        && b[..8].iter().all(u8::is_ascii_digit)
        && b[8] == b'T'
        && b[9..15].iter().all(u8::is_ascii_digit)
        && b[15] == b'Z';
    let suffix_ok = suffix
        .is_none_or(|s| !s.is_empty() && s.len() <= 6 && s.bytes().all(|c| c.is_ascii_digit()) && !s.starts_with('0'));
    base_ok && suffix_ok
}

/// Target duration: the longest segment, rounded, and never below the
/// configured length.
pub(crate) fn playlist(segments: &[SegEntry], min_target: u64, ended: bool) -> String {
    let target = segments.iter().map(|s| s.duration.round() as u64).max().unwrap_or(0).max(min_target).max(1);
    let mut o = String::with_capacity(128 + segments.len() * 48);
    o.push_str("#EXTM3U\n#EXT-X-VERSION:7\n");
    let _ = writeln!(o, "#EXT-X-TARGETDURATION:{target}");
    o.push_str("#EXT-X-MEDIA-SEQUENCE:0\n");
    o.push_str(if ended { "#EXT-X-PLAYLIST-TYPE:VOD\n" } else { "#EXT-X-PLAYLIST-TYPE:EVENT\n" });
    o.push_str("#EXT-X-INDEPENDENT-SEGMENTS\n#EXT-X-MAP:URI=\"init.mp4\"\n");
    for s in segments {
        if s.discontinuity {
            o.push_str("#EXT-X-DISCONTINUITY\n");
        }
        if let Some(pdt) = s.pdt {
            let _ = writeln!(o, "#EXT-X-PROGRAM-DATE-TIME:{}", rfc3339(pdt));
        }
        let _ = writeln!(o, "#EXTINF:{:.5},\n{}", s.duration, segment_name(s.index));
    }
    if ended {
        o.push_str("#EXT-X-ENDLIST\n");
    }
    o
}

/// `(file, seconds)` for every segment a playlist we wrote lists.
pub(crate) fn playlist_segments(text: &str) -> Vec<(String, f64)> {
    let mut out = Vec::new();
    let mut dur = None;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("#EXTINF:") {
            dur = v.split(',').next().and_then(|d| d.trim().parse::<f64>().ok());
        } else if !line.starts_with('#') && !line.is_empty() {
            if let Some(d) = dur.take() {
                out.push((line.to_owned(), d));
            }
        }
    }
    out
}

/// Turns a live (`EVENT`) playlist into a finished one.
pub(crate) fn close_playlist(text: &str) -> String {
    let mut o = text.replace("#EXT-X-PLAYLIST-TYPE:EVENT", "#EXT-X-PLAYLIST-TYPE:VOD");
    if !o.contains("#EXT-X-ENDLIST") {
        if !o.ends_with('\n') {
            o.push('\n');
        }
        o.push_str("#EXT-X-ENDLIST\n");
    }
    o
}

fn civil(days: i64) -> (i64, i64, i64) {
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn split(t: SystemTime) -> (i64, i64, i64, i64, u32) {
    let d = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let (days, sod) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, day) = civil(days);
    (y, m, day, sod, d.subsec_millis())
}

/// `2026-09-17T21:38:00.123Z`.
pub(crate) fn rfc3339(t: SystemTime) -> String {
    let (y, m, d, sod, ms) = split(t);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{ms:03}Z", sod / 3600, sod % 3600 / 60, sod % 60)
}

/// `20260917T213800Z`.
pub(crate) fn id_for(t: SystemTime) -> String {
    let (y, m, d, sod, _) = split(t);
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", sod / 3600, sod % 3600 / 60, sod % 60)
}

/// Parses what `rfc3339` writes (fraction optional, `Z` only).
pub(crate) fn parse_rfc3339(s: &str) -> Option<SystemTime> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut dp = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (y, mo, d) = (dp.next()??, dp.next()??, dp.next()??);
    let (hms, frac) = match time.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (time, None),
    };
    let mut tp = hms.splitn(3, ':').map(|p| p.parse::<i64>().ok());
    let (h, mi, se) = (tp.next()??, tp.next()??, tp.next()??);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let ms = match frac {
        Some(f) if !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()) => {
            let f = &f[..f.len().min(3)];
            f.parse::<u64>().ok()? * 10u64.pow(3 - f.len() as u32)
        }
        Some(_) => return None,
        None => 0,
    };
    let secs = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se;
    let secs = u64::try_from(secs).ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs) + Duration::from_millis(ms))
}

/// Writes `data` to `path` through a temp file and a rename, so readers
/// (and a crash) only ever see the old or the new version.
pub(crate) async fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension().and_then(|e| e.to_str()).unwrap_or("")
    ));
    let res = async {
        tokio::fs::write(&tmp, data).await?;
        tokio::fs::rename(&tmp, path).await
    }
    .await;
    if res.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    res
}

pub(crate) async fn read_meta(dir: &Path) -> Option<Meta> {
    let bytes = tokio::fs::read(dir.join("meta.json")).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids() {
        assert!(valid_id("20260918T101500Z"));
        assert!(valid_id("20260918T101500Z-2"));
        for bad in ["", "..", "20260918T101500", "20260918T101500Z-", "20260918T101500Z-0", "2026091xT101500Z", "a/b"] {
            assert!(!valid_id(bad), "{bad}");
        }
        let t = SystemTime::UNIX_EPOCH + Duration::from_millis(1_789_000_123_456);
        assert!(valid_id(&id_for(t)));
    }

    #[test]
    fn dates_round_trip() {
        for ms in [0u64, 1_789_000_123_456, 951_782_400_000 /* 2000-02-29 */] {
            let t = SystemTime::UNIX_EPOCH + Duration::from_millis(ms);
            assert_eq!(parse_rfc3339(&rfc3339(t)), Some(t), "{}", rfc3339(t));
        }
        assert_eq!(rfc3339(SystemTime::UNIX_EPOCH + Duration::from_secs(1_789_000_000)), "2026-09-10T00:26:40.000Z");
        assert_eq!(id_for(SystemTime::UNIX_EPOCH + Duration::from_secs(1_789_000_000)), "20260910T002640Z");
        assert_eq!(parse_rfc3339("2020-01-01T00:00:00Z"), Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_577_836_800)));
        assert_eq!(parse_rfc3339("garbage"), None);
    }

    #[test]
    fn segment_names() {
        assert_eq!(segment_name(7), "seg-000007.m4s");
        assert_eq!(parse_segment_name("seg-000007.m4s"), Some(7));
        for bad in ["seg-7.m4s", "seg-00000a.m4s", "../seg-000001.m4s", "seg-000001.m4s.tmp"] {
            assert_eq!(parse_segment_name(bad), None, "{bad}");
        }
    }

    #[test]
    fn playlist_shape() {
        let segs = vec![
            SegEntry { index: 1, duration: 2.0, discontinuity: false, pdt: Some(SystemTime::UNIX_EPOCH) },
            SegEntry { index: 2, duration: 2.4, discontinuity: true, pdt: Some(SystemTime::UNIX_EPOCH) },
        ];
        let live = playlist(&segs, 2, false);
        assert!(live.contains("#EXT-X-PLAYLIST-TYPE:EVENT"));
        assert!(!live.contains("ENDLIST"));
        let done = close_playlist(&live);
        assert_eq!(done, playlist(&segs, 2, true));
        assert_eq!(playlist_segments(&done), vec![("seg-000001.m4s".into(), 2.0), ("seg-000002.m4s".into(), 2.4)]);
    }
}
