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

/// No reconnect grace: a dropped publisher ends the playlist at once.
const CFG: HlsConfig =
    HlsConfig { part_ms: 200, segment_ms: 2000, cue_tags: true, cue_out_tags: false, reconnect_grace: Duration::ZERO };

fn fixture() -> Demuxed {
    demux(include_bytes!("../tests/fixtures/av.mp4"))
}

/// 256x144, 30 fps, GOP 60, H.264 + Opus (48 kHz mono), ffmpeg's WHIP-style
/// output for a WebRTC publisher.
fn fixture_opus() -> Demuxed {
    demux(include_bytes!("../tests/fixtures/av_opus.mp4"))
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
async fn h264_opus_init_segment_and_multivariant() {
    let fx = fixture_opus();
    let reg = Registry::new();
    let app = router(reg.clone(), CFG);
    let p = publish(&reg, "opus", &fx, BufferConfig::default()).await;
    push_loops(&p, &fx, 0..1);
    wait_playlist(&app, "opus", |pl| pl.contains("#EXT-X-PART:")).await;

    let r = get(&app, "/hls/opus/init.mp4").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.windows(4).any(|w| w == b"Opus"), "no Opus sample entry:\n{:?}", r.body);
    assert!(r.body.windows(4).any(|w| w == b"dOps"), "no dOps box:\n{:?}", r.body);

    let mut buf = &r.body[..];
    Ftyp::decode(&mut buf).unwrap();
    let moov = Moov::decode(&mut buf).unwrap();
    assert!(buf.is_empty());
    match &moov.trak[1].mdia.minf.stbl.stsd.codecs[0] {
        mp4_atom::Codec::Opus(o) => {
            assert_eq!(o.audio.channel_count, fx.tracks[1].audio.unwrap().channels as u16);
            assert_eq!(o.dops.input_sample_rate, 48_000);
        }
        c => panic!("audio sample entry is {c:?}"),
    }
    assert_eq!(moov.trak[1].mdia.mdhd.timescale, 48_000);

    let master = get(&app, "/hls/opus/master.m3u8").await;
    assert_eq!(master.status, StatusCode::OK);
    assert!(master.text().contains("opus"), "{}", master.text());
    drop(p);
}

/// An Opus-only publish (no video track at all): Opus becomes the primary
/// track and still cuts segments and parts on its own clock.
#[tokio::test]
async fn audio_only_opus_stream() {
    let fx = fixture_opus();
    let audio_track = fx.tracks[1].clone();
    assert_eq!(audio_track.codec, caudal_core::Codec::Opus);
    let reg = Registry::new();
    let app = router(reg.clone(), CFG);
    let p = reg.publish("opus-only", BufferConfig::default()).unwrap();
    p.set_tracks(vec![audio_track]).unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    for n in 0..2 {
        for f in fx.looped(n).filter(|f| f.track == fx.tracks[1].id) {
            p.push(f).unwrap();
        }
    }
    let pl = wait_playlist(&app, "opus-only", |pl| pl.contains("#EXTINF")).await;
    assert!(pl.contains("#EXT-X-PART:"), "{pl}");

    let r = get(&app, "/hls/opus-only/init.mp4").await;
    assert_eq!(r.status, StatusCode::OK);
    let mut buf = &r.body[..];
    Ftyp::decode(&mut buf).unwrap();
    let moov = Moov::decode(&mut buf).unwrap();
    assert_eq!(moov.trak.len(), 1, "audio-only: one track");
    assert!(matches!(moov.trak[0].mdia.minf.stbl.stsd.codecs[0], mp4_atom::Codec::Opus(_)));

    let master = get(&app, "/hls/opus-only/master.m3u8").await;
    assert!(master.text().contains("CODECS=\"opus\""), "{}", master.text());
    drop(p);
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
fn family_root_splits_on_the_first_plus() {
    assert_eq!(family_root("main"), "main");
    assert_eq!(family_root("main+480p"), "main");
    assert_eq!(family_root("main+480p+extra"), "main");
}

#[test]
fn render_master_formats_attrs_and_preserves_caller_order() {
    let variants = vec![
        (
            "main+hi".to_string(),
            VariantAttrs {
                codecs: "avc1.64001f,mp4a.40.2".to_string(),
                resolution: Some((1280, 720)),
                frame_rate: Some(29.97),
                peak: 2_000_000,
                average: Some(1_500_000),
            },
        ),
        (
            "main+lo".to_string(),
            VariantAttrs {
                codecs: "avc1.42001f".to_string(),
                resolution: None,
                frame_rate: None,
                peak: 500_000,
                average: None,
            },
        ),
    ];
    let m = render_master(&variants);
    assert!(m.starts_with("#EXTM3U\n#EXT-X-VERSION:9\n#EXT-X-INDEPENDENT-SEGMENTS\n"), "{m}");
    // Caller order is kept: sorting by bandwidth is `master()`'s job, not
    // `render_master`'s.
    assert!(m.find("main+hi").unwrap() < m.find("main+lo").unwrap(), "{m}");
    assert!(
        m.contains(
            "#EXT-X-STREAM-INF:BANDWIDTH=2000000,AVERAGE-BANDWIDTH=1500000,CODECS=\"avc1.64001f,mp4a.40.2\",RESOLUTION=1280x720,FRAME-RATE=29.970\n../main+hi/index.m3u8\n"
        ),
        "{m}"
    );
    assert!(
        m.contains("#EXT-X-STREAM-INF:BANDWIDTH=500000,CODECS=\"avc1.42001f\"\n../main+lo/index.m3u8\n"),
        "no AVERAGE-BANDWIDTH/RESOLUTION/FRAME-RATE when unmeasured or unknown: {m}"
    );
}

#[tokio::test]
async fn master_aggregates_the_family_and_playlists_report_each_other_never_self() {
    let fx = fixture();
    let reg = Registry::new();
    let app = router(reg.clone(), CFG);
    let root = publish(&reg, "abr", &fx, BufferConfig::default()).await;
    let low = publish(&reg, "abr+low", &fx, BufferConfig::default()).await;
    push_loops(&root, &fx, 0..3);
    push_loops(&low, &fx, 0..3);
    wait_playlist(&app, "abr", |pl| pl.contains("#EXTINF")).await;
    wait_playlist(&app, "abr+low", |pl| pl.contains("#EXTINF")).await;

    // The family root's master lists both renditions, relative URIs, tokens
    // propagated, highest bandwidth first.
    let master = get(&app, "/hls/abr/master.m3u8?token=tkn.val.sig").await;
    assert_eq!(master.status, StatusCode::OK);
    let mtext = master.text();
    assert!(mtext.contains("../abr/index.m3u8?token=tkn.val.sig"), "{mtext}");
    assert!(mtext.contains("../abr+low/index.m3u8?token=tkn.val.sig"), "{mtext}");
    let infs = tag_values(&mtext, "#EXT-X-STREAM-INF:");
    assert_eq!(infs.len(), 2, "{mtext}");
    let bandwidths: Vec<u64> = infs.iter().map(|l| attr(l, "BANDWIDTH").unwrap().parse().unwrap()).collect();
    assert!(bandwidths[0] >= bandwidths[1], "highest bandwidth first: {mtext}");
    assert!(infs.iter().all(|l| attr(l, "AVERAGE-BANDWIDTH").is_some()), "measured over completed segments: {mtext}");
    assert!(infs.iter().all(|l| attr(l, "CODECS").is_some_and(|c| c.contains("avc1."))), "{mtext}");

    // A master request for a rendition name itself stays single-variant.
    let single = get(&app, "/hls/abr+low/master.m3u8").await;
    assert_eq!(single.status, StatusCode::OK);
    let stext = single.text();
    assert_eq!(tag_values(&stext, "#EXT-X-STREAM-INF:").len(), 1, "{stext}");
    assert!(stext.contains("../abr+low/index.m3u8"), "{stext}");
    assert!(!stext.contains("../abr/index.m3u8"), "not aggregated when the name is itself a rendition: {stext}");

    // Each rendition's media playlist reports the other, and only the
    // other: never a report about itself (Apple -50099).
    let root_pl = get(&app, "/hls/abr/index.m3u8?token=tkn.val.sig").await.text();
    let root_reports = tag_values(&root_pl, "#EXT-X-RENDITION-REPORT:");
    assert_eq!(root_reports.len(), 1, "{root_pl}");
    assert!(root_reports[0].contains("URI=\"../abr+low/index.m3u8?token=tkn.val.sig\""), "{root_pl}");
    assert!(attr(root_reports[0], "LAST-MSN").is_some() && attr(root_reports[0], "LAST-PART").is_some(), "{root_pl}");
    assert!(!root_pl.contains("URI=\"../abr/index.m3u8"), "a media playlist never reports on itself:\n{root_pl}");

    let low_pl = get(&app, "/hls/abr+low/index.m3u8").await.text();
    let low_reports = tag_values(&low_pl, "#EXT-X-RENDITION-REPORT:");
    assert_eq!(low_reports.len(), 1, "{low_pl}");
    assert!(low_reports[0].contains("URI=\"../abr/index.m3u8\""), "{low_pl}");
    assert!(!low_pl.contains("URI=\"../abr+low/index.m3u8"), "a media playlist never reports on itself:\n{low_pl}");

    drop(root);
    drop(low);
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
    // OpusHead, stereo, 48 kHz, no pre-skip/gain: RFC 6381 codec string is
    // just "opus".
    const OPUS_HEAD: &[u8] = b"OpusHead\x01\x02\x00\x00\x80\xbb\x00\x00\x00\x00\x00";
    assert_eq!(crate::packager::codec_string(&t(Codec::Opus, OPUS_HEAD)).unwrap(), "opus");
    // A malformed OpusHead (bad magic) yields no codec string, same as a
    // config record that fails to parse for the other codecs.
    assert!(crate::packager::codec_string(&t(Codec::Opus, b"not-an-opus-head!!!")).is_none());
}

fn cue(at_us: i64, kind: caudal_core::CueKind) -> caudal_core::Cue {
    let pts = caudal_scte35::us_to_ticks(at_us) as u64;
    let section = caudal_scte35::build(kind, Some(pts), 1, caudal_scte35::Command::TimeSignal).unwrap();
    caudal_core::Cue { at_us, section, kind }
}

/// Drives the packager with a wall clock that tracks media time exactly,
/// starting at t0, so every date in the playlist is predictable.
fn packager_with(cfg: HlsConfig, loops: std::ops::Range<i64>, cues: &[(i64, caudal_core::Cue)]) -> Packager {
    let fx = fixture();
    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
    let mut pkg = Packager::new(cfg);
    pkg.set_tracks(&fx.tracks);
    for n in loops {
        for f in fx.looped(n) {
            let us = fx.micros(&f);
            pkg.push(&f, t0 + Duration::from_micros(us as u64));
            // A cue is pushed once media has reached the time given with it.
            for (after, c) in cues {
                if f.track.0 == 0 && (us..us + 33_334).contains(after) {
                    pkg.push_cue(c);
                }
            }
        }
    }
    pkg
}

#[test]
fn cues_become_dateranges_dated_from_their_segment() {
    use caudal_core::CueKind;
    let out = cue(4_500_000, CueKind::Out { duration_us: Some(30_000_000) });
    let inn = cue(7_250_000, CueKind::In);
    let pkg = packager_with(CFG, 0..3, &[(4_500_000, out.clone()), (7_250_000, inn.clone())]);
    let pl = pkg.playlist();

    let out_line = format!(
        "#EXT-X-DATERANGE:ID=\"scte35-1\",START-DATE=\"1970-01-01T00:16:44.500Z\",PLANNED-DURATION=30.000,SCTE35-OUT={}",
        caudal_scte35::to_hex(&out.section)
    );
    let in_line = format!(
        "#EXT-X-DATERANGE:ID=\"scte35-1\",START-DATE=\"1970-01-01T00:16:44.500Z\",DURATION=2.750,SCTE35-IN={}",
        caudal_scte35::to_hex(&inn.section)
    );
    // Each right after the PDT of the segment its cue falls in (s2 starts
    // at 4 s, s3 at 6 s); START-DATE = that PDT + the offset into it.
    assert!(pl.contains(&format!("#EXT-X-PROGRAM-DATE-TIME:1970-01-01T00:16:44.000Z\n{out_line}\n")), "{pl}");
    assert!(pl.contains(&format!("#EXT-X-PROGRAM-DATE-TIME:1970-01-01T00:16:46.000Z\n{in_line}\n")), "{pl}");
    assert_eq!(tag_values(&pl, "#EXT-X-DATERANGE:").len(), 2, "{pl}");
    // The hex round-trips to the same cue.
    let hex = attr(tag_values(&pl, "#EXT-X-DATERANGE:")[0], "SCTE35-OUT").unwrap();
    let parsed = caudal_scte35::parse(&caudal_scte35::from_hex(hex).unwrap()).unwrap();
    assert_eq!(parsed.kind, CueKind::Out { duration_us: Some(30_000_000) });

    // Off by config.
    let off = packager_with(
        HlsConfig { cue_tags: false, ..CFG },
        0..3,
        &[(4_500_000, out.clone()), (7_250_000, inn.clone())],
    );
    assert!(!off.playlist().contains("DATERANGE"));
}

#[test]
fn legacy_cue_out_tags_mark_the_break() {
    use caudal_core::CueKind;
    let out = cue(4_500_000, CueKind::Out { duration_us: Some(30_000_000) });
    let inn = cue(7_250_000, CueKind::In);
    let cfg = HlsConfig { cue_tags: false, cue_out_tags: true, ..CFG };
    let pl = packager_with(cfg, 0..3, &[(4_500_000, out), (7_250_000, inn)]).playlist();

    // s2 starts at 4 s (the break starts inside it), s3 at 6 s (inside the
    // break, which ends at 7.25 s): CONT first, then the splice-in.
    assert!(pl.contains("#EXT-X-PROGRAM-DATE-TIME:1970-01-01T00:16:44.000Z\n#EXT-X-CUE-OUT:DURATION=30.000\n"), "{pl}");
    assert!(
        pl.contains(
            "#EXT-X-PROGRAM-DATE-TIME:1970-01-01T00:16:46.000Z\n#EXT-X-CUE-OUT-CONT:ElapsedTime=1.500,Duration=30.000\n#EXT-X-CUE-IN\n"
        ),
        "{pl}"
    );
    assert_eq!(pl.matches("#EXT-X-CUE-OUT-CONT").count(), 1, "only s3 is inside the break:\n{pl}");
    assert!(!pl.contains("DATERANGE"), "cue_tags off:\n{pl}");
}

#[test]
fn dateranges_leave_with_the_window_but_not_before_their_end() {
    use caudal_core::CueKind;
    let other = cue(1_000_000, CueKind::Other);
    let out = cue(4_500_000, CueKind::Out { duration_us: Some(30_000_000) });
    // 32 s of media: the window (6 full segments) starts past 18 s.
    let pkg = packager_with(CFG, 0..8, &[(1_000_000, other), (4_500_000, out)]);
    let pl = pkg.playlist();
    assert!(!pl.contains("SCTE35-CMD"), "a 1 s cue is gone once its segment left:\n{pl}");
    // The break (4.5 s + 30 s) still overlaps the window: kept, and listed
    // before the first segment it is older than.
    let lines: Vec<&str> = pl.lines().collect();
    let first_pdt = lines.iter().position(|l| l.starts_with("#EXT-X-PROGRAM-DATE-TIME")).unwrap();
    assert!(lines[first_pdt + 1].contains("SCTE35-OUT"), "{pl}");
}

#[tokio::test]
async fn a_pushed_cue_reaches_the_live_playlist() {
    use caudal_core::CueKind;
    let fx = fixture();
    let reg = Registry::new();
    let app = router(reg.clone(), CFG);
    let p = publish(&reg, "ad", &fx, BufferConfig::default()).await;
    push_loops(&p, &fx, 0..2);
    let c = cue(4_500_000, CueKind::Out { duration_us: Some(15_000_000) });
    p.push_cue(c.clone()).unwrap();
    push_loops(&p, &fx, 2..3);
    let pl = wait_playlist(&app, "ad", |pl| pl.contains("SCTE35-OUT") && pl.contains("s4.m4s")).await;
    let lines: Vec<&str> = pl.lines().collect();
    let at = lines.iter().position(|l| l.starts_with("#EXT-X-DATERANGE:")).unwrap();
    let dr = lines[at];
    assert_eq!(attr(dr, "PLANNED-DURATION"), Some("15.000"));
    assert_eq!(attr(dr, "SCTE35-OUT"), Some(caudal_scte35::to_hex(&c.section).as_str()));
    // START-DATE = the PDT of the segment it sits under (s2, 4 s) + 0.5 s.
    let pdt = lines[at - 1].strip_prefix("#EXT-X-PROGRAM-DATE-TIME:").expect("right after a PDT");
    let ms = |s: &str| -> i64 {
        let t = &s[11..23]; // HH:MM:SS.mmm
        let h: i64 = t[0..2].parse().unwrap();
        let m: i64 = t[3..5].parse().unwrap();
        let sec: i64 = t[6..8].parse().unwrap();
        let milli: i64 = t[9..12].parse().unwrap();
        ((h * 60 + m) * 60 + sec) * 1000 + milli
    };
    let start = attr(dr, "START-DATE").unwrap();
    assert_eq!((ms(start) - ms(pdt)).rem_euclid(86_400_000), 500, "{pl}");
    assert!(lines[at + 1].starts_with("#EXT-X-PART") || lines[at + 1].starts_with("#EXTINF"), "{pl}");
    drop(p);
}

/// Grace on: a republish of the same name continues the playlist.
const GRACE: HlsConfig = HlsConfig { reconnect_grace: Duration::from_secs(10), ..CFG };

/// Every segment number listed (full segments and parts), in order.
fn listed_msns(pl: &str) -> Vec<u64> {
    let mut v: Vec<u64> = pl
        .lines()
        .filter_map(|l| {
            let uri = if l.starts_with('#') { attr(l.strip_prefix("#EXT-X-PART:")?, "URI")? } else { l };
            parse_media_name(uri).map(|(m, _)| m)
        })
        .collect();
    v.dedup();
    v
}

fn media_sequence(pl: &str) -> u64 {
    tag_values(pl, "#EXT-X-MEDIA-SEQUENCE:")[0].parse().unwrap()
}

/// The text after `#EXT-X-DISCONTINUITY`: the tags that open the first
/// segment of the new publish, and what follows.
fn after_discontinuity(pl: &str) -> &str {
    pl.split("#EXT-X-DISCONTINUITY\n").nth(1).unwrap_or_else(|| panic!("no discontinuity:\n{pl}"))
}

/// Feeds `loops` of `fx` from media time zero, like a fresh encoder
/// connection.
fn feed(pkg: &mut Packager, fx: &Demuxed, loops: std::ops::Range<i64>) {
    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
    for n in loops {
        for f in fx.looped(n) {
            pkg.push(&f, t0 + Duration::from_micros(fx.micros(&f) as u64));
        }
    }
}

#[test]
fn resume_keeps_counting_and_marks_the_discontinuity() {
    let fx = fixture();
    let mut pkg = Packager::new(GRACE);
    pkg.set_tracks(&fx.tracks);
    feed(&mut pkg, &fx, 0..2);
    pkg.suspend();
    let gap = pkg.playlist();
    assert!(!gap.contains("#EXT-X-ENDLIST"), "no ENDLIST while a republish may come:\n{gap}");
    assert!(gap.contains("#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"s4.p0.m4s\""), "{gap}");
    assert_eq!(listed_msns(&gap), vec![0, 1, 2, 3], "{gap}");

    // The new encoder's clock starts over at zero.
    pkg.resume();
    pkg.set_tracks(&fx.tracks);
    feed(&mut pkg, &fx, 0..1);
    let pl = pkg.playlist();
    assert_eq!(media_sequence(&pl), 0);
    assert_eq!(listed_msns(&pl), vec![0, 1, 2, 3, 4, 5], "numbers keep counting:\n{pl}");
    assert_eq!(pl.matches("#EXT-X-DISCONTINUITY\n").count(), 1, "{pl}");
    let first_new = after_discontinuity(&pl);
    assert!(first_new.starts_with("#EXT-X-PROGRAM-DATE-TIME:"), "{pl}");
    let part = first_new.lines().find(|l| l.starts_with("#EXT-X-PART:")).unwrap();
    assert!(part.contains("URI=\"s4.p0.m4s\"") && part.contains("INDEPENDENT=YES"), "{pl}");
    assert_eq!(pl.matches("#EXT-X-MAP:").count(), 1, "same init: one EXT-X-MAP:\n{pl}");
    assert!(!pl.contains("#EXT-X-DISCONTINUITY-SEQUENCE"), "{pl}");

    // Once the discontinuity leaves the window, DISCONTINUITY-SEQUENCE counts it.
    feed(&mut pkg, &fx, 1..5);
    let pl = pkg.playlist();
    assert!(media_sequence(&pl) > 4, "{pl}");
    assert!(pl.contains("#EXT-X-DISCONTINUITY-SEQUENCE:1\n"), "{pl}");
    assert!(!pl.contains("#EXT-X-DISCONTINUITY\n"), "{pl}");
    let msns = listed_msns(&pl);
    assert!(msns.windows(2).all(|w| w[1] == w[0] + 1), "{pl}");
}

#[test]
fn resume_with_a_new_init_segment_adds_an_ext_x_map() {
    let fx = fixture();
    let opus = fixture_opus();
    let mut pkg = Packager::new(GRACE);
    pkg.set_tracks(&fx.tracks);
    feed(&mut pkg, &fx, 0..2);
    let first_init = pkg.init.clone().unwrap();
    pkg.suspend();
    // The encoder came back with another audio codec.
    pkg.resume();
    pkg.set_tracks(&opus.tracks);
    feed(&mut pkg, &opus, 0..1);
    let pl = pkg.playlist();
    assert_eq!(listed_msns(&pl), vec![0, 1, 2, 3, 4, 5], "{pl}");
    assert!(pl.contains("#EXT-X-INDEPENDENT-SEGMENTS\n#EXT-X-MAP:URI=\"init.mp4\"\n"), "{pl}");
    assert!(after_discontinuity(&pl).starts_with("#EXT-X-MAP:URI=\"init1.mp4\"\n"), "{pl}");
    assert_eq!(pkg.init_for(0).unwrap(), first_init, "old segments keep their init");
    assert_eq!(pkg.init_for(1), pkg.init);
    assert_ne!(pkg.init_for(0), pkg.init_for(1));

    // When the old segments leave, so does their init segment.
    feed(&mut pkg, &opus, 1..5);
    let pl = pkg.playlist();
    assert!(pl.contains("#EXT-X-INDEPENDENT-SEGMENTS\n#EXT-X-MAP:URI=\"init1.mp4\"\n"), "{pl}");
    assert_eq!(pl.matches("#EXT-X-MAP:").count(), 1, "{pl}");
    assert!(pkg.init_for(0).is_none());
}

#[test]
fn init_names() {
    assert_eq!(parse_init_name("init1.mp4"), Some(1));
    assert_eq!(parse_init_name("init12.mp4"), Some(12));
    for bad in ["init.mp4", "init0.mp4", "init01.mp4", "initx.mp4", "init1.m4s", "init-1.mp4"] {
        assert_eq!(parse_init_name(bad), None, "{bad}");
    }
    assert_eq!(packager::init_uri(0), "init.mp4");
    assert_eq!(packager::init_uri(3), "init3.mp4");
}

#[tokio::test]
async fn republish_within_the_grace_continues_the_same_playlist() {
    let fx = fixture();
    let reg = Registry::new();
    let app = router(reg.clone(), GRACE);
    let p = publish(&reg, "re", &fx, BufferConfig::default()).await;
    push_loops(&p, &fx, 0..2);
    let live = wait_playlist(&app, "re", |pl| pl.contains("s2.p")).await;
    drop(p);
    // The tail is flushed, but the playlist stays live.
    let gap = wait_playlist(&app, "re", |pl| pl.contains("\ns3.m4s")).await;
    assert!(!gap.contains("#EXT-X-ENDLIST") && gap.contains("PRELOAD-HINT"), "{gap}");
    assert!(media_sequence(&gap) >= media_sequence(&live));

    let p = publish(&reg, "re", &fx, BufferConfig::default()).await;
    push_loops(&p, &fx, 0..2);
    let pl = wait_playlist(&app, "re", |pl| pl.contains("\ns6.m4s")).await;
    // Six full segments in the window: s0 slid out, the rest kept counting.
    assert_eq!(listed_msns(&pl), (1..=7).collect::<Vec<_>>(), "{pl}");
    assert_eq!(media_sequence(&pl), 1);
    assert_eq!(pl.matches("#EXT-X-DISCONTINUITY\n").count(), 1, "{pl}");
    assert_eq!(listed_msns(after_discontinuity(&pl))[0], 4, "the new publish starts at s4:\n{pl}");
    assert!(!pl.contains("#EXT-X-ENDLIST"), "{pl}");
    // Old and new segments are both served under their own numbers.
    for f in ["s3.m4s", "s4.m4s", "s4.p0.m4s", "init.mp4"] {
        assert_eq!(get(&app, &format!("/hls/re/{f}")).await.status, StatusCode::OK, "{f}");
    }
    drop(p);
}

#[tokio::test]
async fn blocking_reload_during_the_gap_is_answered_after_republish() {
    let fx = fixture();
    let reg = Registry::new();
    let app = router(reg.clone(), GRACE);
    let p = publish(&reg, "gap", &fx, BufferConfig::default()).await;
    push_loops(&p, &fx, 0..2);
    wait_playlist(&app, "gap", |pl| pl.contains("s2.p")).await;
    drop(p);
    let gap = wait_playlist(&app, "gap", |pl| pl.contains("\ns3.m4s")).await;
    let hint = tag_values(&gap, "#EXT-X-PRELOAD-HINT:")[0];
    assert_eq!(attr(hint, "URI"), Some("s4.p0.m4s"), "{gap}");

    let app2 = app.clone();
    let reload = tokio::spawn(async move { get(&app2, "/hls/gap/index.m3u8?_HLS_msn=4&_HLS_part=0").await });
    let app3 = app.clone();
    let hinted = tokio::spawn(async move { get(&app3, "/hls/gap/s4.p0.m4s").await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!reload.is_finished() && !hinted.is_finished(), "must wait for the publisher to come back");

    let p = publish(&reg, "gap", &fx, BufferConfig::default()).await;
    push_loops(&p, &fx, 0..1);
    let r = reload.await.unwrap();
    assert_eq!(r.status, StatusCode::OK);
    assert!(after_discontinuity(&r.text()).contains("URI=\"s4.p0.m4s\""), "{}", r.text());
    let h = hinted.await.unwrap();
    assert_eq!(h.status, StatusCode::OK);
    assert!(h.body.windows(4).any(|w| w == b"moof"));
    drop(p);
}

#[tokio::test(start_paused = true)]
async fn endlist_only_after_the_grace_runs_out() {
    let fx = fixture();
    let reg = Registry::new();
    let app = router(reg.clone(), GRACE);
    let p = publish(&reg, "late", &fx, BufferConfig::default()).await;
    push_loops(&p, &fx, 0..2);
    wait_playlist(&app, "late", |pl| pl.contains("s2.p")).await;
    drop(p);
    wait_playlist(&app, "late", |pl| pl.contains("\ns3.m4s")).await;
    tokio::time::sleep(Duration::from_secs(9)).await;
    let pl = get(&app, "/hls/late/index.m3u8").await.text();
    assert!(!pl.contains("#EXT-X-ENDLIST"), "still inside the grace:\n{pl}");
    tokio::time::sleep(Duration::from_secs(2)).await;
    let pl = get(&app, "/hls/late/index.m3u8").await.text();
    assert!(pl.contains("#EXT-X-ENDLIST") && !pl.contains("PRELOAD-HINT"), "{pl}");

    // Too late: the next publish starts a playlist of its own, as before.
    let p = publish(&reg, "late", &fx, BufferConfig::default()).await;
    push_loops(&p, &fx, 0..1);
    let pl = wait_playlist(&app, "late", |pl| !pl.contains("#EXT-X-ENDLIST") && pl.contains("s0.p")).await;
    assert!(!pl.contains("#EXT-X-DISCONTINUITY"), "{pl}");
    drop(p);
}
