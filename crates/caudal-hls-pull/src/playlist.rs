//! A small HLS playlist parser (RFC 8216 and the LL-HLS additions of its
//! successor draft): just what a pull needs. A master playlist gives its
//! variants; a media playlist gives its whole segments with their media
//! sequence numbers, the target duration, `EXT-X-ENDLIST`, and the tags a
//! pull cannot play (`EXT-X-MAP` for fMP4, `EXT-X-KEY` encryption).
//!
//! LL-HLS partial segments (`EXT-X-PART`, `EXT-X-PRELOAD-HINT`) are
//! skipped: every part ends up in a whole segment, which is what is
//! fetched. Unknown tags are ignored, as the RFC asks.

use reqwest::Url;

#[derive(Debug, Clone, PartialEq)]
pub enum Playlist {
    Master(Vec<Variant>),
    Media(Media),
}

/// One `EXT-X-STREAM-INF` of a master playlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variant {
    pub bandwidth: u64,
    pub uri: Url,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Media {
    /// `EXT-X-TARGETDURATION`, seconds.
    pub target_duration: f64,
    /// Every whole segment, in order; `seq` counts up from
    /// `EXT-X-MEDIA-SEQUENCE`.
    pub segments: Vec<Segment>,
    pub endlist: bool,
    /// `EXT-X-MAP` URI: the segments are fMP4 (or carry an init section).
    pub map: Option<Url>,
    /// An `EXT-X-KEY` with a method other than `NONE`.
    pub encrypted: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub seq: u64,
    pub uri: Url,
    /// `EXTINF`, seconds.
    pub duration: f64,
    /// An `EXT-X-DISCONTINUITY` precedes this segment.
    pub discontinuity: bool,
}

/// The highest-`BANDWIDTH` variant (the first of equals).
pub fn best_variant(variants: &[Variant]) -> Option<&Variant> {
    variants.iter().rev().max_by_key(|v| v.bandwidth)
}

/// Parses `text`, resolving URIs against `base` (the playlist's own URL).
pub fn parse(text: &str, base: &Url) -> Result<Playlist, String> {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    if lines.next().map(|l| l.trim_start_matches('\u{feff}')) != Some("#EXTM3U") {
        return Err("not an HLS playlist (no #EXTM3U)".into());
    }
    let join = |uri: &str| base.join(uri).map_err(|e| format!("segment uri {uri:?}: {e}"));

    let mut variants = Vec::new();
    let mut pending_variant: Option<u64> = None;
    let mut target_duration = None;
    let mut seq = 0u64;
    let mut segments = Vec::new();
    let mut endlist = false;
    let mut map = None;
    let mut encrypted = false;
    let mut extinf: Option<f64> = None;
    let mut discontinuity = false;
    let mut is_media = false;

    for line in lines {
        if let Some(tag) = line.strip_prefix('#') {
            let (name, value) = tag.split_once(':').unwrap_or((tag, ""));
            match name {
                "EXT-X-STREAM-INF" => {
                    let bw = attr(value, "BANDWIDTH")
                        .and_then(|b| b.parse().ok())
                        .ok_or_else(|| format!("EXT-X-STREAM-INF without a BANDWIDTH: {line:?}"))?;
                    pending_variant = Some(bw);
                }
                "EXT-X-TARGETDURATION" => {
                    is_media = true;
                    target_duration =
                        Some(value.parse::<f64>().map_err(|_| format!("bad EXT-X-TARGETDURATION: {line:?}"))?);
                }
                "EXT-X-MEDIA-SEQUENCE" => {
                    seq = value.parse().map_err(|_| format!("bad EXT-X-MEDIA-SEQUENCE: {line:?}"))?;
                }
                "EXTINF" => {
                    is_media = true;
                    let d = value.split(',').next().unwrap_or("").trim();
                    extinf = Some(d.parse().map_err(|_| format!("bad EXTINF: {line:?}"))?);
                }
                "EXT-X-DISCONTINUITY" => discontinuity = true,
                "EXT-X-ENDLIST" => endlist = true,
                "EXT-X-MAP" => {
                    let uri = attr(value, "URI").ok_or_else(|| format!("EXT-X-MAP without a URI: {line:?}"))?;
                    map = Some(join(&uri)?);
                }
                "EXT-X-KEY" => {
                    encrypted |= attr(value, "METHOD").is_some_and(|m| m != "NONE");
                }
                _ => {}
            }
            continue;
        }
        // A URI line: a variant's playlist or a segment.
        if let Some(bandwidth) = pending_variant.take() {
            variants.push(Variant { bandwidth, uri: join(line)? });
        } else if let Some(duration) = extinf.take() {
            segments.push(Segment { seq, uri: join(line)?, duration, discontinuity });
            seq += 1;
            discontinuity = false;
        }
    }

    if !variants.is_empty() {
        return Ok(Playlist::Master(variants));
    }
    if !is_media {
        return Err("neither a master playlist (no EXT-X-STREAM-INF) nor a media playlist".into());
    }
    let target_duration = target_duration.ok_or("media playlist without EXT-X-TARGETDURATION")?;
    if !(target_duration > 0.0 && target_duration.is_finite()) {
        return Err(format!("EXT-X-TARGETDURATION {target_duration} is not a positive number"));
    }
    Ok(Playlist::Media(Media { target_duration, segments, endlist, map, encrypted }))
}

/// The value of `key` in an attribute list (`KEY=VALUE,KEY="quoted, value"`),
/// unquoted.
fn attr(list: &str, key: &str) -> Option<String> {
    let mut rest = list;
    while !rest.is_empty() {
        let (k, after) = rest.split_once('=')?;
        let (value, next) = if let Some(q) = after.strip_prefix('"') {
            let end = q.find('"')?;
            (&q[..end], q[end + 1..].trim_start_matches(','))
        } else {
            match after.split_once(',') {
                Some((v, n)) => (v, n),
                None => (after, ""),
            }
        };
        if k.trim() == key {
            return Some(value.to_owned());
        }
        rest = next;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("http://h/live/index.m3u8").unwrap()
    }

    #[test]
    fn attributes() {
        let l = r#"BANDWIDTH=1280000,CODECS="avc1.64001f,mp4a.40.2",RESOLUTION=1280x720"#;
        assert_eq!(attr(l, "BANDWIDTH").as_deref(), Some("1280000"));
        assert_eq!(attr(l, "CODECS").as_deref(), Some("avc1.64001f,mp4a.40.2"));
        assert_eq!(attr(l, "RESOLUTION").as_deref(), Some("1280x720"));
        assert_eq!(attr(l, "FRAME-RATE"), None);
        assert_eq!(attr("METHOD=NONE", "METHOD").as_deref(), Some("NONE"));
    }

    #[test]
    fn master_picks_highest_bandwidth() {
        let text = "#EXTM3U\n#EXT-X-VERSION:3\n\
            #EXT-X-STREAM-INF:BANDWIDTH=800000,CODECS=\"avc1.4d401e,mp4a.40.2\",RESOLUTION=640x360\nlow/index.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080\nhttps://cdn.example/hi.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=2000000\nmid.m3u8\n";
        let Playlist::Master(v) = parse(text, &base()).unwrap() else { panic!("master") };
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].uri.as_str(), "http://h/live/low/index.m3u8");
        assert_eq!(v[2].uri.as_str(), "http://h/live/mid.m3u8");
        assert_eq!(best_variant(&v).unwrap().uri.as_str(), "https://cdn.example/hi.m3u8");
    }

    #[test]
    fn ffmpeg_live_media_playlist() {
        let text = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:7\n\
            #EXTINF:2.000000,\nindex7.ts\n#EXTINF:1.960000,\nindex8.ts\n\
            #EXT-X-DISCONTINUITY\n#EXTINF:2.000000,\n/abs/index9.ts?x=1\n";
        let Playlist::Media(m) = parse(text, &base()).unwrap() else { panic!("media") };
        assert_eq!(m.target_duration, 2.0);
        assert!(!m.endlist && m.map.is_none() && !m.encrypted);
        let seqs: Vec<u64> = m.segments.iter().map(|s| s.seq).collect();
        assert_eq!(seqs, [7, 8, 9]);
        assert_eq!(m.segments[0].uri.as_str(), "http://h/live/index7.ts");
        assert_eq!(m.segments[2].uri.as_str(), "http://h/abs/index9.ts?x=1");
        assert_eq!(m.segments[1].duration, 1.96);
        let disc: Vec<bool> = m.segments.iter().map(|s| s.discontinuity).collect();
        assert_eq!(disc, [false, false, true]);
    }

    #[test]
    fn endlist_map_key_and_ll_hls_parts() {
        let text = "#EXTM3U\r\n#EXT-X-TARGETDURATION:4\r\n#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK=1.0\r\n\
            #EXT-X-PART-INF:PART-TARGET=0.2\r\n#EXT-X-MAP:URI=\"init.mp4\"\r\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\"\r\n\
            #EXT-X-PROGRAM-DATE-TIME:2026-11-21T10:00:00Z\r\n\
            #EXT-X-PART:DURATION=0.2,URI=\"s0.p0.mp4\",INDEPENDENT=YES\r\n#EXTINF:4.0,\r\ns0.mp4\r\n\
            #EXT-X-PART:DURATION=0.2,URI=\"s1.p0.mp4\"\r\n#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"s1.p1.mp4\"\r\n\
            #EXT-X-ENDLIST\r\n";
        let Playlist::Media(m) = parse(text, &base()).unwrap() else { panic!("media") };
        assert_eq!(m.segments.len(), 1, "parts are not segments");
        assert_eq!(m.segments[0].seq, 0, "no EXT-X-MEDIA-SEQUENCE: starts at 0");
        assert!(m.endlist && m.encrypted);
        assert_eq!(m.map.unwrap().as_str(), "http://h/live/init.mp4");
        let none = parse("#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-KEY:METHOD=NONE\n", &base()).unwrap();
        assert!(matches!(none, Playlist::Media(m) if !m.encrypted && m.segments.is_empty()));
    }

    #[test]
    fn refusals() {
        assert!(parse("<html>", &base()).unwrap_err().contains("#EXTM3U"));
        assert!(parse("#EXTM3U\n#EXT-X-VERSION:3\n", &base()).is_err());
        assert!(parse("#EXTM3U\n#EXTINF:1,\na.ts\n", &base()).unwrap_err().contains("TARGETDURATION"));
        assert!(parse("#EXTM3U\n#EXT-X-TARGETDURATION:0\n", &base()).is_err());
        assert!(parse("#EXTM3U\n#EXT-X-TARGETDURATION:x\n", &base()).is_err());
        assert!(
            parse("#EXTM3U\n#EXT-X-STREAM-INF:RESOLUTION=1x1\na.m3u8\n", &base()).unwrap_err().contains("BANDWIDTH")
        );
    }
}
