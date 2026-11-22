//! Classic HLS (TS segments): the segmenter on its own, then the router
//! end to end with ffmpeg as the judge, for H.264 + AAC (the `av.mp4`
//! fixture) and for MPEG-2 video + MP2 audio (ffmpeg writes a broadcast
//! style TS, `caudal_ts` demuxes it, Caudal re-segments it).

use std::process::Command;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use caudal_core::{BufferConfig, Codec, Frame, Registry, TrackInfo, TrackKind};
use tower::ServiceExt;

use super::tests::mp4demux::{Demuxed, demux};
use super::*;
use tsseg::{TS_WINDOW, TsSegmenter};

const CFG: HlsConfig = HlsConfig {
    part_ms: 200,
    segment_ms: 2000,
    cue_tags: true,
    cue_out_tags: false,
    reconnect_grace: Duration::ZERO,
    ts: true,
    ll_dash: super::LlDash::DEFAULT,
};

/// 4 s of 256x144 H.264 (30 fps, GOP 60, B-frames) + 48 kHz AAC.
fn fixture() -> Demuxed {
    demux(include_bytes!("../tests/fixtures/av.mp4"))
}

fn segmenter(fx: &Demuxed, tracks: &[TrackInfo], loops: std::ops::Range<i64>) -> TsSegmenter {
    let mut s = TsSegmenter::new(2000);
    s.set_tracks(tracks);
    push(&mut s, fx, loops);
    s
}

fn push(s: &mut TsSegmenter, fx: &Demuxed, loops: std::ops::Range<i64>) {
    for n in loops {
        for f in fx.looped(n) {
            s.push(&f);
        }
    }
}

fn extinf(pl: &str) -> Vec<f64> {
    pl.lines().filter_map(|l| l.strip_prefix("#EXTINF:")).map(|v| v.trim_end_matches(',').parse().unwrap()).collect()
}

fn tag<'a>(pl: &'a str, t: &str) -> Option<&'a str> {
    pl.lines().find_map(|l| l.strip_prefix(t))
}

fn pid(p: &[u8]) -> u16 {
    (u16::from(p[1] & 0x1F) << 8) | u16::from(p[2])
}

fn payload_start(p: &[u8]) -> bool {
    p[1] & 0x40 != 0
}

fn random_access(p: &[u8]) -> bool {
    p[3] & 0x20 != 0 && p[4] > 0 && p[5] & 0x40 != 0
}

/// Every segment is whole 188-byte packets and opens with PAT, PMT, then
/// the start of a keyframe PES on the video PID.
fn assert_starts_clean(seg: &[u8], first_media_pid: u16, keyframe_flag: bool) {
    assert_eq!(seg.len() % 188, 0);
    let pk: Vec<&[u8]> = seg.chunks(188).collect();
    assert!(pk.iter().all(|p| p[0] == 0x47), "sync bytes");
    assert_eq!(pid(pk[0]), 0, "PAT first");
    assert_eq!(pid(pk[1]), 0x1000, "PMT second");
    assert_eq!(pid(pk[2]), first_media_pid);
    assert!(payload_start(pk[2]));
    if keyframe_flag {
        assert!(random_access(pk[2]), "a keyframe opens the segment");
    }
}

#[test]
fn ts_names() {
    assert_eq!(parse_ts_name("t0.ts"), Some(0));
    assert_eq!(parse_ts_name("t42.ts"), Some(42));
    for bad in ["t.ts", "ts.m3u8", "s1.m4s", "t1.m4s", "t-1.ts", "x1.ts", "t1x.ts"] {
        assert_eq!(parse_ts_name(bad), None, "{bad}");
    }
    // None of the LL-HLS names parse as a TS segment, and the reverse.
    for ll in ["index.m3u8", "master.m3u8", "subs.m3u8", "init.mp4", "init1.mp4", "s3.m4s", "s3.p1.m4s", "c3.vtt"] {
        assert_eq!(parse_ts_name(ll), None, "{ll}");
    }
    assert_eq!(parse_media_name("t3.ts"), None);
}

#[test]
fn version_3_playlist_cut_on_keyframes() {
    let fx = fixture();
    let mut s = segmenter(&fx, &fx.tracks, 0..3);
    let live = s.playlist();
    assert!(live.starts_with("#EXTM3U\n#EXT-X-VERSION:3\n"), "{live}");
    assert_eq!(tag(&live, "#EXT-X-TARGETDURATION:"), Some("2"));
    assert_eq!(tag(&live, "#EXT-X-MEDIA-SEQUENCE:"), Some("0"));
    assert!(!live.contains("#EXT-X-ENDLIST"));
    assert!(!live.contains("DISCONTINUITY"), "{live}");
    // 12 s of GOP-60 video at 30 fps: 2 s segments, the last still open.
    assert_eq!(extinf(&live), vec![2.0; 5], "{live}");
    assert!(live.contains("#EXTINF:2.000,\nt0.ts\n"), "{live}");
    // No LL-HLS tag leaks in.
    for t in ["EXT-X-PART", "EXT-X-MAP", "PRELOAD-HINT", "SERVER-CONTROL", "INDEPENDENT-SEGMENTS"] {
        assert!(!live.contains(t), "{t} in {live}");
    }

    s.end();
    let done = s.playlist();
    assert_eq!(extinf(&done), vec![2.0; 6], "{done}");
    assert!(done.ends_with("t5.ts\n#EXT-X-ENDLIST\n"), "{done}");
    for seg in &s.segments {
        assert_starts_clean(&seg.data, 0x101, true);
    }
}

#[test]
fn window_slides_and_memory_stays_bounded() {
    let fx = fixture();
    let s = segmenter(&fx, &fx.tracks, 0..10);
    let pl = s.playlist();
    // 40 s: 19 closed segments, the last 6 listed.
    assert_eq!(extinf(&pl).len(), TS_WINDOW);
    assert_eq!(tag(&pl, "#EXT-X-MEDIA-SEQUENCE:"), Some("13"), "{pl}");
    assert!(pl.contains("t18.ts"));
    assert!(s.segments.len() <= TS_WINDOW + 2, "held: {}", s.segments.len());
    assert!(s.segment(0).is_none(), "slid out long ago");
    assert!(s.segment(12).is_some(), "just slid out: still fetchable");
    assert!(s.segment(19).is_none(), "still open");
    assert_eq!(s.next_seq(), 19);
}

#[test]
fn continuity_counters_run_on_across_segments() {
    let fx = fixture();
    let mut s = segmenter(&fx, &fx.tracks, 0..3);
    s.end();
    let all: Vec<u8> = s.segments.iter().flat_map(|seg| seg.data.iter().copied()).collect();
    let mut last: HashMap<u16, u8> = HashMap::new();
    for p in all.chunks(188) {
        if p[3] & 0x10 == 0 {
            continue; // no payload: the counter does not advance
        }
        let cc = p[3] & 0x0F;
        if let Some(prev) = last.insert(pid(p), cc) {
            assert_eq!(cc, (prev + 1) & 0x0F, "pid {:#x}", pid(p));
        }
    }
    assert!(last.contains_key(&0x101) && last.contains_key(&0x102), "video and audio PIDs");
}

#[test]
fn republish_and_jumps_are_discontinuities() {
    let fx = fixture();
    let mut s = segmenter(&fx, &fx.tracks, 0..2);
    // The publisher drops and comes back with timestamps from zero again.
    s.suspend();
    s.set_tracks(&fx.tracks);
    push(&mut s, &fx, 0..1);
    let pl = s.playlist();
    // 0..3 from the first publish (the open one closed at the drop), then
    // the second publish's first segment after a discontinuity.
    assert!(pl.contains("t3.ts\n#EXT-X-DISCONTINUITY\n#EXTINF:2.000,\nt4.ts\n"), "{pl}");
    assert_eq!(pl.matches("#EXT-X-DISCONTINUITY\n").count(), 1);
    assert!(tag(&pl, "#EXT-X-DISCONTINUITY-SEQUENCE:").is_none());

    // Timestamp jumps on the same publish are too: forward 72 s (loop 20
    // after loop 0), then back 76 s (loop 1 after loop 20).
    push(&mut s, &fx, 20..21);
    push(&mut s, &fx, 1..2);
    let pl = s.playlist();
    assert_eq!(pl.matches("#EXT-X-DISCONTINUITY\n").count(), 3, "{pl}");
    for seq in [4, 6, 8] {
        assert!(pl.contains(&format!("#EXT-X-DISCONTINUITY\n#EXTINF:2.000,\nt{seq}.ts\n")), "{seq}: {pl}");
    }

    // Once they have slid out, the sequence counts them.
    push(&mut s, &fx, 2..6);
    let pl = s.playlist();
    assert_eq!(tag(&pl, "#EXT-X-MEDIA-SEQUENCE:"), Some("11"), "{pl}");
    assert!(!pl.contains("#EXT-X-DISCONTINUITY\n"), "{pl}");
    assert_eq!(tag(&pl, "#EXT-X-DISCONTINUITY-SEQUENCE:"), Some("3"), "{pl}");
    for seg in &s.segments {
        assert_starts_clean(&seg.data, 0x101, true);
    }
}

#[test]
fn idle_drops_segments_but_keeps_counting() {
    let fx = fixture();
    let mut s = segmenter(&fx, &fx.tracks, 0..2);
    s.idle();
    assert!(s.segments.is_empty());
    let next = s.next_seq();
    s.set_tracks(&fx.tracks);
    push(&mut s, &fx, 2..4);
    let pl = s.playlist();
    assert_eq!(tag(&pl, "#EXT-X-MEDIA-SEQUENCE:"), Some(next.to_string().as_str()), "{pl}");
    assert!(pl.contains(&format!("#EXT-X-DISCONTINUITY\n#EXTINF:2.000,\nt{next}.ts\n")), "{pl}");
}

#[test]
fn audio_only_segments_start_with_psi() {
    let fx = fixture();
    let audio: Vec<TrackInfo> = fx.tracks.iter().filter(|t| t.kind() == TrackKind::Audio).cloned().collect();
    assert_eq!(audio.len(), 1);
    let mut s = segmenter(&fx, &audio, 0..3);
    s.end();
    let durations = extinf(&s.playlist());
    assert!(durations.len() >= 5, "{durations:?}");
    for d in &durations[..durations.len() - 1] {
        assert!((2.0..2.05).contains(d), "audio frames cut just past 2 s: {durations:?}");
    }
    for seg in &s.segments {
        assert_starts_clean(&seg.data, 0x102, false);
    }
}

#[test]
fn opus_only_stream_yields_nothing() {
    let fx = demux(include_bytes!("../tests/fixtures/av_opus.mp4"));
    let opus: Vec<TrackInfo> = fx.tracks.iter().filter(|t| t.codec == Codec::Opus).cloned().collect();
    let mut s = segmenter(&fx, &opus, 0..2);
    s.end();
    assert!(s.segments.is_empty());
}

// ---- through the router, checked by ffmpeg ----

fn have(bin: &str) -> bool {
    Command::new(bin).arg("-version").output().is_ok_and(|o| o.status.success())
}

struct Reply {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Bytes,
}

async fn get(app: &Router, path: &str) -> Reply {
    let res = app.clone().oneshot(Request::get(path).body(Body::empty()).unwrap()).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    Reply { status, headers, body }
}

/// Publishes `frames`, asking for the TS playlist first so its segmenter
/// is running, then ends the stream and returns the final playlist and
/// every segment it lists, written to a fresh temp dir as a local copy.
async fn publish_and_fetch(
    name: &str,
    tracks: Vec<TrackInfo>,
    frames: Vec<Frame>,
) -> (String, Vec<(String, Bytes)>, std::path::PathBuf) {
    let reg = Registry::new();
    let app = router(reg.clone(), CFG, Vec::new());
    let p = reg.publish(name, BufferConfig::default()).unwrap();
    p.set_tracks(tracks).unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Starts the segmenter; answers once the first segment exists.
    let first = tokio::spawn({
        let app = app.clone();
        let path = format!("/hls/{name}/ts.m3u8?token=abc.def");
        async move { get(&app, &path).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    for f in frames {
        p.push(f).unwrap();
    }
    let first = first.await.unwrap();
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(first.headers[header::CONTENT_TYPE], PLAYLIST);
    let text = String::from_utf8_lossy(&first.body).into_owned();
    assert!(text.contains("t0.ts?token=abc.def\n"), "token carried to segments: {text}");

    drop(p);
    let t0 = Instant::now();
    let pl = loop {
        let r = get(&app, &format!("/hls/{name}/ts.m3u8")).await;
        let text = String::from_utf8_lossy(&r.body).into_owned();
        if r.status == StatusCode::OK && text.contains("#EXT-X-ENDLIST") {
            break text;
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "never ended: {} {text}", r.status);
        tokio::time::sleep(Duration::from_millis(20)).await;
    };

    let dir = std::env::temp_dir().join(format!("caudal_ts_hls_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("ts.m3u8"), &pl).unwrap();
    let mut segs = Vec::new();
    for uri in pl.lines().filter(|l| !l.starts_with('#')) {
        let r = get(&app, &format!("/hls/{name}/{uri}")).await;
        assert_eq!(r.status, StatusCode::OK, "{uri}");
        assert_eq!(r.headers[header::CONTENT_TYPE], "video/mp2t");
        std::fs::write(dir.join(uri), &r.body).unwrap();
        segs.push((uri.to_owned(), r.body));
    }
    assert_eq!(get(&app, &format!("/hls/{name}/t999.ts")).await.status, StatusCode::NOT_FOUND);
    (pl, segs, dir)
}

/// `ffmpeg -xerror` decodes `input` to nothing; any decode error fails.
fn decodes(input: &std::path::Path) -> Result<(), String> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-xerror", "-i"])
        .arg(input)
        .args(["-f", "null", "-"])
        .output()
        .expect("run ffmpeg");
    if out.status.success() && out.stderr.is_empty() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

fn ffprobe(input: &std::path::Path, entries: &str) -> String {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", entries, "-of", "compact=p=0"])
        .arg(input)
        .output()
        .expect("run ffprobe");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Every segment alone, then the whole playlist through ffmpeg's HLS
/// demuxer; returns the playlist's stream summary.
fn judge(pl: &str, segs: &[(String, Bytes)], dir: &std::path::Path) -> String {
    for (uri, _) in segs {
        decodes(&dir.join(uri)).unwrap_or_else(|e| panic!("{uri} does not decode on its own: {e}"));
    }
    let playlist = dir.join("ts.m3u8");
    decodes(&playlist).unwrap_or_else(|e| panic!("playlist does not decode: {e}\n{pl}"));
    // Timestamps run on across segment boundaries: the playlist's packets
    // never go backwards in decode order, and no gap opens between
    // segments (ffmpeg reports the stream's span as the sum of EXTINFs).
    let dts = ffprobe(&playlist, "packet=stream_index,dts_time");
    let mut last: HashMap<String, f64> = HashMap::new();
    for line in dts.lines() {
        let mut idx = None;
        let mut t = None;
        for kv in line.split('|') {
            match kv.split_once('=') {
                Some(("stream_index", v)) => idx = Some(v.to_owned()),
                Some(("dts_time", v)) => t = v.parse::<f64>().ok(),
                _ => {}
            }
        }
        let (Some(idx), Some(t)) = (idx, t) else { continue };
        if let Some(prev) = last.insert(idx.clone(), t) {
            assert!(t > prev, "stream {idx}: dts {t} after {prev}");
        }
    }
    let span: f64 = extinf(pl).iter().sum();
    let probed: f64 = ffprobe(&playlist, "format=duration")
        .trim()
        .strip_prefix("duration=")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0);
    assert!((probed - span).abs() < 0.5, "duration {probed} vs EXTINF total {span}");
    ffprobe(&playlist, "stream=codec_name,width,height,sample_rate")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn h264_aac_ts_segments_play_in_ffmpeg() {
    if !have("ffmpeg") || !have("ffprobe") {
        eprintln!("SKIP: ffmpeg/ffprobe not installed");
        return;
    }
    let fx = fixture();
    let frames: Vec<Frame> = (0..3).flat_map(|n| fx.looped(n).collect::<Vec<_>>()).collect();
    let (pl, segs, dir) = publish_and_fetch("tsavc", fx.tracks.clone(), frames).await;
    assert_eq!(extinf(&pl), vec![2.0; 6], "{pl}");
    for (_, seg) in &segs {
        assert_starts_clean(seg, 0x101, true);
    }
    let streams = judge(&pl, &segs, &dir);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(streams.contains("codec_name=h264|width=256|height=144"), "{streams}");
    assert!(streams.contains("codec_name=aac"), "{streams}");
}

/// MPEG-2 720p59.94 + MP2, as an ATSC chain carries it, demuxed by
/// `caudal_ts` (what the SRT / TS-over-UDP ingest does).
fn mpeg2_source() -> Option<(Vec<TrackInfo>, Vec<Frame>)> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=60000/1001:duration=6"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=6"])
        .args(["-c:v", "mpeg2video", "-b:v", "8M", "-g", "30", "-bf", "2", "-c:a", "mp2", "-b:a", "128k"])
        .args(["-f", "mpegts", "pipe:1"])
        .output()
        .ok()?;
    assert!(out.status.success(), "ffmpeg: {}", String::from_utf8_lossy(&out.stderr));
    use caudal_ts::demux::{DemuxEvent, Demuxer};
    let mut ts = caudal_ts::ts::TsDemux::new();
    let mut demuxer = Demuxer::new();
    let (mut units, mut events) = (Vec::new(), Vec::new());
    for chunk in out.stdout.chunks(1316) {
        ts.feed(chunk);
        ts.drain(&mut units);
    }
    ts.flush(&mut units);
    for unit in units {
        demuxer.consume(unit, &mut events);
    }
    let (mut tracks, mut frames) = (Vec::new(), Vec::new());
    for ev in events {
        match ev {
            DemuxEvent::VideoInit(t) | DemuxEvent::AudioInit(t) => tracks.push(t),
            DemuxEvent::VideoFrame(f) | DemuxEvent::AudioFrame(f) => frames.push(f),
            DemuxEvent::Cue(_) => {}
        }
    }
    Some((tracks, frames))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mpeg2_mp2_ts_segments_play_in_ffmpeg() {
    if !have("ffmpeg") || !have("ffprobe") {
        eprintln!("SKIP: ffmpeg/ffprobe not installed");
        return;
    }
    let (tracks, frames) = mpeg2_source().expect("ffmpeg ran");
    let codecs: Vec<Codec> = tracks.iter().map(|t| t.codec).collect();
    assert!(codecs.contains(&Codec::Mpeg2Video) && codecs.contains(&Codec::Mp2), "{codecs:?}");
    let (pl, segs, dir) = publish_and_fetch("tsmpeg2", tracks, frames).await;
    // 6 s, an I picture every 30 frames (~0.5 s): ~2 s segments.
    let durations = extinf(&pl);
    assert!(durations.len() >= 3, "{pl}");
    for d in &durations[..durations.len() - 1] {
        assert!((1.9..2.6).contains(d), "{pl}");
    }
    for (_, seg) in &segs {
        assert_starts_clean(seg, 0x101, true);
    }
    let streams = judge(&pl, &segs, &dir);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(streams.contains("codec_name=mpeg2video|width=1280|height=720"), "{streams}");
    assert!(streams.contains("codec_name=mp2"), "{streams}");
}

#[tokio::test]
async fn ts_off_is_404_and_ll_hls_untouched() {
    let fx = fixture();
    let reg = Registry::new();
    let app = router(reg.clone(), HlsConfig { ts: false, ..CFG }, Vec::new());
    let p = reg.publish("off", BufferConfig::default()).unwrap();
    p.set_tracks(fx.tracks.clone()).unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    for f in fx.looped(0) {
        p.push(f).unwrap();
    }
    assert_eq!(get(&app, "/hls/off/ts.m3u8").await.status, StatusCode::NOT_FOUND);
    assert_eq!(get(&app, "/hls/off/t0.ts").await.status, StatusCode::NOT_FOUND);
    let ll = get(&app, "/hls/off/index.m3u8").await;
    assert_eq!(ll.status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&ll.body).contains("#EXT-X-VERSION:9"));
}

#[tokio::test]
async fn ts_playlist_of_unknown_stream_is_404() {
    let reg = Registry::new();
    let app = router(reg.clone(), CFG, Vec::new());
    assert_eq!(get(&app, "/hls/nobody/ts.m3u8").await.status, StatusCode::NOT_FOUND);
}
