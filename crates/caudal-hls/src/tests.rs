//! Packager tests: a real `Registry`, real H.264 + AAC frames from a
//! fixture ffmpeg wrote (256x144 testsrc2, 30 fps, GOP 60, 48 kHz AAC,
//! B-frames on), and the real router answering real requests.

#[path = "mp4demux.rs"]
mod mp4demux;

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use caudal_core::{BufferConfig, Publisher, Registry};
use mp4_atom::{Any, Atom, Decode, Ftyp, Moof, Moov};
use tower::ServiceExt;

use super::*;
use mp4demux::{Demuxed, demux};

const CFG: HlsConfig = HlsConfig { part_ms: 200, segment_ms: 2000 };

fn fixture() -> Demuxed {
    demux(include_bytes!("../tests/fixtures/av.mp4"))
}

struct Reply {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Bytes,
}

impl Reply {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

async fn get(app: &Router, path: &str) -> Reply {
    let res = app.clone().oneshot(Request::get(path).body(Body::empty()).unwrap()).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    Reply { status, headers, body }
}

/// Polls the playlist until `pred` holds.
async fn wait_playlist(app: &Router, name: &str, pred: impl Fn(&str) -> bool) -> String {
    let t0 = Instant::now();
    loop {
        let r = get(app, &format!("/hls/{name}/index.m3u8")).await;
        if r.status == StatusCode::OK && pred(&r.text()) {
            return r.text();
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "playlist never matched; last:\n{}", r.text());
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Publishes and lets the router's listener start the packager, as it
/// would in the time between an RTMP publish and its first frame.
async fn publish(reg: &Arc<Registry>, name: &str, fx: &Demuxed, cfg: BufferConfig) -> Publisher {
    let p = reg.publish(name, cfg).unwrap();
    p.set_tracks(fx.tracks.clone()).unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    p
}

fn push_loops(p: &Publisher, fx: &Demuxed, loops: std::ops::Range<i64>) {
    for n in loops {
        for f in fx.looped(n) {
            p.push(f).unwrap();
        }
    }
}

fn tag_values<'a>(pl: &'a str, tag: &str) -> Vec<&'a str> {
    pl.lines().filter_map(|l| l.strip_prefix(tag)).collect()
}

fn attr<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split(',').find_map(|kv| kv.strip_prefix(key).and_then(|v| v.strip_prefix('='))).map(|v| v.trim_matches('"'))
}

/// `(msn, part, duration, independent)` for every EXT-X-PART line.
fn parts(pl: &str) -> Vec<(u64, usize, f64, bool)> {
    tag_values(pl, "#EXT-X-PART:")
        .into_iter()
        .map(|l| {
            let uri = attr(l, "URI").unwrap();
            let (msn, part) = parse_media_name(uri).unwrap();
            (msn, part.unwrap(), attr(l, "DURATION").unwrap().parse().unwrap(), l.contains("INDEPENDENT=YES"))
        })
        .collect()
}

#[test]
fn media_names() {
    assert_eq!(parse_media_name("s12.m4s"), Some((12, None)));
    assert_eq!(parse_media_name("s12.p3.m4s"), Some((12, Some(3))));
    for bad in ["12.m4s", "s.m4s", "s1.p.m4s", "s1.px.m4s", "s-1.m4s", "s1.mp4"] {
        assert_eq!(parse_media_name(bad), None, "{bad}");
    }
}

#[test]
fn program_date_time_format() {
    let t = SystemTime::UNIX_EPOCH + Duration::from_millis(1_789_695_480_123);
    assert_eq!(packager::rfc3339(t), "2026-09-18T01:38:00.123Z");
    assert_eq!(packager::rfc3339(SystemTime::UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
    let leap = SystemTime::UNIX_EPOCH + Duration::from_secs(951_782_400); // 2000-02-29
    assert_eq!(packager::rfc3339(leap), "2000-02-29T00:00:00.000Z");
}

#[tokio::test]
async fn init_segment_is_decodable_cmaf() {
    let fx = fixture();
    let reg = Registry::new();
    let app = router(reg.clone(), CFG);
    let p = publish(&reg, "init", &fx, BufferConfig::default()).await;
    push_loops(&p, &fx, 0..1);
    wait_playlist(&app, "init", |pl| pl.contains("#EXT-X-PART:")).await;

    let r = get(&app, "/hls/init/init.mp4").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers[header::CONTENT_TYPE], "video/mp4");
    assert_eq!(r.headers[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
    assert_eq!(&r.body[4..8], b"ftyp");
    let mut buf = &r.body[..];
    Ftyp::decode(&mut buf).unwrap();
    let moov = Moov::decode(&mut buf).unwrap();
    assert!(buf.is_empty());
    assert_eq!(moov.trak.len(), 2);
    assert_eq!(moov.mvex.as_ref().unwrap().trex.len(), 2);
    match &moov.trak[0].mdia.minf.stbl.stsd.codecs[0] {
        mp4_atom::Codec::Avc1(a) => {
            let mut avcc = Vec::new();
            a.avcc.encode_body(&mut avcc).unwrap();
            assert_eq!(avcc, fx.tracks[0].init.to_vec(), "avcC round-trips byte for byte");
            assert_eq!((a.visual.width, a.visual.height), (256, 144));
        }
        c => panic!("video sample entry is {c:?}"),
    }
    assert_eq!(moov.trak[0].mdia.mdhd.timescale, 90_000);
    match &moov.trak[1].mdia.minf.stbl.stsd.codecs[0] {
        mp4_atom::Codec::Mp4a(m) => {
            let asc = &m.esds.es_desc.dec_config.dec_specific.as_ref().unwrap().raw;
            assert_eq!(asc[..], fx.tracks[1].init[..]);
        }
        c => panic!("audio sample entry is {c:?}"),
    }
    assert_eq!(moov.trak[1].mdia.mdhd.timescale, 48_000);
}

#[tokio::test]
async fn parts_and_segments_follow_the_config() {
    let fx = fixture();
    let reg = Registry::new();
    let app = router(reg.clone(), CFG);
    let p = publish(&reg, "cut", &fx, BufferConfig::default()).await;
    push_loops(&p, &fx, 0..3); // 12 s: segments 0..=4 complete, 5 open
    let pl = wait_playlist(&app, "cut", |pl| pl.contains("#EXTINF") && pl.contains("s4.m4s")).await;

    for tag in [
        "#EXTM3U",
        "#EXT-X-VERSION:9",
        "#EXT-X-TARGETDURATION:2",
        "#EXT-X-MAP:URI=\"init.mp4\"",
        "#EXT-X-MEDIA-SEQUENCE:0",
    ] {
        assert!(pl.contains(tag), "missing {tag}:\n{pl}");
    }
    assert!(pl.contains("#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK=0.601"), "{pl}");
    assert!(pl.contains("#EXT-X-PART-INF:PART-TARGET=0.200"), "{pl}");
    assert!(pl.contains("#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"s5.p"), "{pl}");
    assert!(!pl.contains("RENDITION-REPORT"), "a media playlist never reports on itself: {pl}");
    assert_eq!(tag_values(&pl, "#EXT-X-PROGRAM-DATE-TIME:").len(), 6, "one PDT per segment:\n{pl}");

    // Segments: 2 s each, cut on keyframes.
    let extinf: Vec<f64> =
        tag_values(&pl, "#EXTINF:").iter().map(|v| v.trim_end_matches(',').parse().unwrap()).collect();
    assert_eq!(extinf.len(), 5, "{pl}");
    for d in &extinf {
        assert!((d - 2.0).abs() < 1e-6, "segment of {d} s:\n{pl}");
    }

    // Parts: never above the target, full-size except where a segment ends,
    // INDEPENDENT=YES exactly on the first part of each segment.
    let ps = parts(&pl);
    assert!(!ps.is_empty());
    for &(msn, i, d, ind) in &ps {
        assert!(d <= 0.2 + 1e-9, "s{msn}.p{i} is {d} s");
        assert!(
            d >= 0.85 * 0.2 || ps.iter().all(|&(m, j, ..)| m != msn || j <= i),
            "short part s{msn}.p{i} ({d} s) is not last"
        );
        assert_eq!(ind, i == 0, "s{msn}.p{i} INDEPENDENT={ind}");
    }
    // Parts for complete segments sum to the segment.
    let s3: f64 = ps.iter().filter(|p| p.0 == 3).map(|p| p.2).sum();
    assert!((s3 - 2.0).abs() < 1e-6, "parts of s3 sum to {s3}");
    assert!(ps.iter().all(|p| p.0 >= 3), "only the last 3 segments list parts:\n{pl}");

    // Each part is one moof + mdat; the independent one starts on a sync
    // sample and carries both tracks.
    let r = get(&app, "/hls/cut/s4.p0.m4s").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers[header::CONTENT_TYPE], "video/iso.segment");
    assert_eq!(r.headers[header::CACHE_CONTROL], "max-age=60");
    let mut buf = &r.body[..];
    let moof = Moof::decode(&mut buf).unwrap();
    assert!(matches!(Any::decode(&mut buf).unwrap(), Any::Mdat(_)));
    assert!(buf.is_empty(), "exactly one moof + mdat");
    assert_eq!(moof.traf.len(), 2, "video and audio in the same fragment");
    let v = &moof.traf[0].trun[0].entries;
    assert_eq!(v[0].flags, Some(0x0200_0000), "first sample is sync");
    assert!(v[1..].iter().all(|e| e.flags == Some(0x0101_0000)));
    assert_eq!(v.len(), 6, "200 ms of 30 fps");
    let tfdt = moof.traf[0].tfdt.as_ref().unwrap().base_media_decode_time;
    assert_eq!(tfdt, (10 + 8) * 90_000, "s4 starts at 8 s (+10 s shift)");

    // The full segment is its parts back to back.
    let full = get(&app, "/hls/cut/s3.m4s").await;
    assert_eq!(full.status, StatusCode::OK);
    let mut cat = Vec::new();
    for &(m, i, ..) in ps.iter().filter(|p| p.0 == 3) {
        cat.extend_from_slice(&get(&app, &format!("/hls/cut/s{m}.p{i}.m4s")).await.body);
    }
    assert_eq!(full.body[..], cat[..]);

    // Playlist headers.
    let r = get(&app, "/hls/cut/index.m3u8").await;
    assert_eq!(r.headers[header::CONTENT_TYPE], "application/vnd.apple.mpegurl");
    assert_eq!(r.headers[header::CACHE_CONTROL], "no-cache");
    assert_eq!(r.headers[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
    drop(p);
}

#[tokio::test]
async fn window_slides_and_old_segments_404() {
    let fx = fixture();
    let reg = Registry::new();
    let app = router(reg.clone(), CFG);
    let p = publish(&reg, "win", &fx, BufferConfig::default()).await;
    push_loops(&p, &fx, 0..6); // 24 s: 11 complete segments
    let pl = wait_playlist(&app, "win", |pl| pl.contains("s10.m4s")).await;
    assert_eq!(tag_values(&pl, "#EXTINF:").len(), 6, "{pl}");
    assert!(pl.contains("#EXT-X-MEDIA-SEQUENCE:5"), "{pl}");
    assert_eq!(get(&app, "/hls/win/s4.m4s").await.status, StatusCode::NOT_FOUND);
    assert_eq!(get(&app, "/hls/win/s4.p0.m4s").await.status, StatusCode::NOT_FOUND);
    assert_eq!(get(&app, "/hls/win/s5.m4s").await.status, StatusCode::OK);
    assert_eq!(get(&app, "/hls/nope/index.m3u8").await.status, StatusCode::NOT_FOUND);
    assert_eq!(get(&app, "/hls/win/other.txt").await.status, StatusCode::NOT_FOUND);
    drop(p);
}

#[tokio::test]
async fn blocking_reload_wakes_when_the_part_lands() {
    let fx = fixture();
    let reg = Registry::new();
    let app = router(reg.clone(), CFG);
    let p = publish(&reg, "blk", &fx, BufferConfig::default()).await;
    // First loop, minus the last frames: segment 1 is open.
    let first: Vec<_> = fx.looped(0).collect();
    let split = first.len() - 20;
    for f in &first[..split] {
        p.push(f.clone()).unwrap();
    }
    let pl = wait_playlist(&app, "blk", |pl| pl.contains("s1.p")).await;
    let hint = tag_values(&pl, "#EXT-X-PRELOAD-HINT:")[0];
    let (msn, part) = parse_media_name(attr(hint, "URI").unwrap()).unwrap();
    let part = part.unwrap();

    // The playlist for the hinted part, and the hinted part itself, block.
    let app2 = app.clone();
    let reload = tokio::spawn(async move {
        let t0 = Instant::now();
        let r = get(&app2, &format!("/hls/blk/index.m3u8?_HLS_msn={msn}&_HLS_part={part}")).await;
        (r, t0.elapsed())
    });
    let app3 = app.clone();
    let hinted = tokio::spawn(async move { get(&app3, &format!("/hls/blk/s{msn}.p{part}.m4s")).await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(!reload.is_finished() && !hinted.is_finished(), "must wait for the part");

    for f in &first[split..] {
        p.push(f.clone()).unwrap();
    }
    push_loops(&p, &fx, 1..2);
    let (r, waited) = reload.await.unwrap();
    assert_eq!(r.status, StatusCode::OK);
    assert!(waited >= Duration::from_millis(150));
    assert!(r.text().contains(&format!("URI=\"s{msn}.p{part}.m4s\"")), "{}", r.text());
    let h = hinted.await.unwrap();
    assert_eq!(h.status, StatusCode::OK);
    assert!(h.body.windows(4).any(|w| w == b"moof"));

    // Already available: answers at once. Too far ahead: 400. Bad query: 400.
    let t0 = Instant::now();
    assert_eq!(get(&app, "/hls/blk/index.m3u8?_HLS_msn=0&_HLS_part=0").await.status, StatusCode::OK);
    assert!(t0.elapsed() < Duration::from_millis(100));
    assert_eq!(get(&app, "/hls/blk/index.m3u8?_HLS_msn=99").await.status, StatusCode::BAD_REQUEST);
    assert_eq!(get(&app, "/hls/blk/index.m3u8?_HLS_part=1").await.status, StatusCode::BAD_REQUEST);
    assert_eq!(get(&app, "/hls/blk/index.m3u8?_HLS_msn=x").await.status, StatusCode::BAD_REQUEST);
    drop(p);
}

#[tokio::test(start_paused = true)]
async fn blocking_reload_times_out_with_503() {
    let fx = fixture();
    let reg = Registry::new();
    let app = router(reg.clone(), CFG);
    let p = publish(&reg, "idle", &fx, BufferConfig::default()).await;
    push_loops(&p, &fx, 0..1);
    let pl = wait_playlist(&app, "idle", |pl| pl.contains("#EXT-X-PART:")).await;
    let hint = tag_values(&pl, "#EXT-X-PRELOAD-HINT:")[0];
    let (msn, part) = parse_media_name(attr(hint, "URI").unwrap()).unwrap();
    // Publisher stalls: nothing new ever lands.
    let t0 = tokio::time::Instant::now();
    let r = get(&app, &format!("/hls/idle/index.m3u8?_HLS_msn={msn}&_HLS_part={}", part.unwrap())).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(t0.elapsed(), Duration::from_secs(6), "3 x target duration");
    drop(p);
}

#[tokio::test]
async fn end_of_stream_appends_endlist() {
    let fx = fixture();
    let reg = Registry::new();
    let app = router(reg.clone(), CFG);
    let p = publish(&reg, "end", &fx, BufferConfig::default()).await;
    push_loops(&p, &fx, 0..2);
    wait_playlist(&app, "end", |pl| pl.contains("s1.m4s")).await;
    drop(p);
    let pl = wait_playlist(&app, "end", |pl| pl.contains("#EXT-X-ENDLIST")).await;
    assert!(!pl.contains("PRELOAD-HINT"), "{pl}");
    // The tail was flushed into a last segment: 8 s → 4 segments.
    assert_eq!(tag_values(&pl, "#EXTINF:").len(), 4, "{pl}");
    // Still served after the publisher left.
    assert_eq!(get(&app, "/hls/end/s3.m4s").await.status, StatusCode::OK);
}

#[tokio::test]
async fn lagging_packager_restarts_on_a_keyframe_with_a_discontinuity() {
    let fx = fixture();
    let reg = Registry::new();
    let app = router(reg.clone(), CFG);
    let small = BufferConfig { window: Duration::from_secs(3), max_bytes: 64 << 20 };
    let p = publish(&reg, "lag", &fx, small).await;
    push_loops(&p, &fx, 0..1);
    wait_playlist(&app, "lag", |pl| pl.contains("s1.p")).await;
    // 20 s arrive while the packager cannot run (single-threaded runtime):
    // the ring evicts what it has not read.
    push_loops(&p, &fx, 1..6);
    let pl = wait_playlist(&app, "lag", |pl| pl.contains("#EXT-X-DISCONTINUITY\n")).await;
    // After the gap, the first segment starts independent.
    let after = pl.split("#EXT-X-DISCONTINUITY\n").nth(1).unwrap();
    assert!(
        after.lines().find(|l| l.starts_with("#EXT-X-PART:")).is_none_or(|l| l.contains("INDEPENDENT=YES")),
        "{pl}"
    );
    drop(p);
}

#[tokio::test]
async fn play_page() {
    let reg = Registry::new();
    let app = router(reg.clone(), CFG);
    let fx = fixture();
    let _p = publish(&reg, "cam", &fx, BufferConfig::default()).await;
    let r = get(&app, "/play/cam").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.headers[header::CONTENT_TYPE].to_str().unwrap().starts_with("text/html"));
    let page = r.text();
    assert!(page.contains("https://cdn.jsdelivr.net/npm/hls.js@1"));
    assert!(page.contains("lowLatencyMode: true"));
    assert!(page.contains("latency"));
    assert_eq!(get(&app, "/play/nobody").await.status, StatusCode::NOT_FOUND);
}

#[test]
fn codec_strings() {
    use bytes::Bytes;
    use caudal_core::{Codec, TrackId, TrackInfo};
    let t = |codec, init: &'static [u8]| TrackInfo {
        id: TrackId(0),
        codec,
        timescale: 90_000,
        init: Bytes::from_static(init),
        lang: None,
        video: None,
        audio: None,
    };
    // High profile, level 3.1.
    assert_eq!(crate::packager::codec_string(&t(Codec::H264, &[1, 0x64, 0x00, 0x1f, 0xff])).unwrap(), "avc1.64001f");
    // AAC-LC, 48 kHz stereo.
    assert_eq!(crate::packager::codec_string(&t(Codec::Aac, &[0x11, 0x90])).unwrap(), "mp4a.40.2");
    // HEVC Main, Main tier, level 3.1 (93), progressive-source constraint.
    let hvcc: &[u8] = &[1, 0x01, 0x60, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 93, 0xf0];
    assert_eq!(crate::packager::codec_string(&t(Codec::H265, hvcc)).unwrap(), "hvc1.1.6.L93.90");
}
