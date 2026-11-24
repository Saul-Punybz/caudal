//! `/mp4/{name}.mp4` through the router: real H.264/H.265 + AAC from
//! ffmpeg, demuxed by caudal_ts the way SRT ingest does, published, read
//! back from the response body, and judged by ffprobe and `ffmpeg -xerror`.

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode, header};
use bytes::Bytes as RawBytes;
use caudal_core::{
    Access, BufferConfig, Codec, Denied, Frame, Gate, GateFuture, Registry, TrackId, TrackInfo, VideoParams,
};
use caudal_ts::demux::{DemuxEvent, Demuxer};
use caudal_ts::ts::TsDemux;
use tower::ServiceExt;

fn have(bin: &str) -> bool {
    Command::new(bin).arg("-version").output().is_ok_and(|o| o.status.success())
}

fn tools() -> bool {
    let ok = have("ffmpeg") && have("ffprobe");
    if !ok {
        eprintln!("SKIP: ffmpeg/ffprobe not installed");
    }
    ok
}

fn has_encoder(name: &str) -> bool {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).split_whitespace().any(|w| w == name))
}

/// Four seconds of MPEG-TS from ffmpeg: test video (1 s GOPs) and a sine.
fn source(video_args: &[&str], audio_args: &[&str]) -> Vec<u8> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error"])
        .args(["-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30:duration=4"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=4"])
        .args(video_args)
        .args(["-g", "30", "-keyint_min", "30"])
        .args(audio_args)
        .args(["-f", "mpegts", "pipe:1"])
        .output()
        .expect("run ffmpeg");
    assert!(out.status.success(), "ffmpeg: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout
}

fn h264_aac() -> Vec<u8> {
    source(
        &["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-bf", "2"],
        &["-ac", "2", "-c:a", "aac", "-b:a", "128k"],
    )
}

struct Demuxed {
    tracks: Vec<TrackInfo>,
    frames: Vec<Frame>,
}

fn demux(ts: &[u8]) -> Demuxed {
    let (mut ts_demux, mut demuxer) = (TsDemux::new(), Demuxer::new());
    let (mut units, mut events) = (Vec::new(), Vec::new());
    for chunk in ts.chunks(1316) {
        ts_demux.feed(chunk);
        ts_demux.drain(&mut units);
    }
    ts_demux.flush(&mut units);
    for unit in units {
        demuxer.consume(unit, &mut events);
    }
    let mut d = Demuxed { tracks: Vec::new(), frames: Vec::new() };
    for ev in events {
        match ev {
            DemuxEvent::VideoInit(t) | DemuxEvent::AudioInit(t) => {
                if !d.tracks.iter().any(|x| x.id == t.id) {
                    d.tracks.push(t);
                }
            }
            DemuxEvent::VideoFrame(f) | DemuxEvent::AudioFrame(f) => d.frames.push(f),
            DemuxEvent::Cue(_) => {}
        }
    }
    d.tracks.sort_by_key(|t| t.id);
    d
}

async fn get(app: &Router, path: &str) -> axum::response::Response {
    app.clone().oneshot(Request::get(path).body(Body::empty()).unwrap()).await.unwrap()
}

async fn body(res: axum::response::Response) -> Bytes {
    tokio::time::timeout(Duration::from_secs(10), axum::body::to_bytes(res.into_body(), usize::MAX))
        .await
        .expect("the body ends when the stream ends")
        .unwrap()
}

/// Publishes `d` as `live`, opens `path` before the first frame, pushes
/// everything, ends the stream, and returns the whole response body.
async fn play_through(d: &Demuxed, path: &str) -> Bytes {
    let reg = Registry::new();
    let app = caudal_mp4::router(reg.clone(), Vec::new());
    let p = reg.publish("live", BufferConfig::default()).unwrap();
    p.set_tracks(d.tracks.clone()).unwrap();
    let res = get(&app, path).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CONTENT_TYPE], "video/mp4");
    assert!(res.headers().get(header::CONTENT_LENGTH).is_none(), "endless: no length");
    let sessions = reg.sessions().list(Some("live"));
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].protocol, "mp4");
    assert_eq!(reg.get("live").unwrap().stats().viewers, 1);
    for f in &d.frames {
        p.push(f.clone()).unwrap();
    }
    drop(p);
    let b = body(res).await;
    // The pump task drops the session right after the body's last chunk.
    for _ in 0..50 {
        if reg.sessions().list(Some("live")).is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(reg.sessions().list(Some("live")).len(), 0, "session closes with the body");
    b
}

fn write_tmp(tag: &str, data: &[u8]) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("caudal_mp4_{tag}_{}.mp4", std::process::id()));
    std::fs::write(&path, data).unwrap();
    path
}

fn ffprobe_streams(path: &std::path::Path) -> String {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "stream=codec_name,width,height,sample_rate,channels", "-of"])
        .arg("compact=p=0")
        .arg(path)
        .output()
        .expect("run ffprobe");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Per stream index, every packet's dts (seconds) in file order.
fn packet_dts(path: &std::path::Path) -> Vec<Vec<f64>> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "packet=stream_index,dts_time", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .expect("run ffprobe");
    let mut per: Vec<Vec<f64>> = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut it = line.split(',');
        let (Some(i), Some(t)) = (it.next(), it.next()) else { continue };
        let (Ok(i), Ok(t)) = (i.trim().parse::<usize>(), t.trim().parse::<f64>()) else { continue };
        if per.len() <= i {
            per.resize(i + 1, Vec::new());
        }
        per[i].push(t);
    }
    per
}

fn decodes(path: &std::path::Path) -> Result<(), String> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-xerror", "-i"])
        .arg(path)
        .args(["-f", "null", "-"])
        .output()
        .expect("run ffmpeg decode");
    if out.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&out.stderr).into_owned()) }
}

/// The top-level box types, in order.
fn boxes(b: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 8 <= b.len() {
        let size = u32::from_be_bytes(b[i..i + 4].try_into().unwrap()) as usize;
        out.push(String::from_utf8_lossy(&b[i + 4..i + 8]).into_owned());
        if size < 8 {
            break;
        }
        i += size;
    }
    assert_eq!(i, b.len(), "the body is whole boxes");
    out
}

/// Probes, checks the timeline and decodes; returns the ffprobe streams.
fn judge(tag: &str, b: &[u8], streams: usize) -> String {
    let path = write_tmp(tag, b);
    let probe = ffprobe_streams(&path);
    let dts = packet_dts(&path);
    let decoded = decodes(&path);
    let _ = std::fs::remove_file(&path);
    assert_eq!(dts.len(), streams, "{probe}");
    for (i, s) in dts.iter().enumerate() {
        assert!(s.len() > 10, "stream {i}: {} packets", s.len());
        assert!(s.windows(2).all(|w| w[1] > w[0]), "stream {i}: dts strictly increasing");
        // Starts at the join point: decode times count from it.
        assert!(s[0].abs() < 0.2, "stream {i} starts at {}", s[0]);
        let span = s.last().unwrap() - s[0];
        assert!(span > 2.5 && span < 4.2, "stream {i}: {span} s");
    }
    decoded.expect("decodes without an error");
    probe
}

#[tokio::test]
async fn h264_and_aac_as_fragmented_mp4() {
    if !tools() {
        return;
    }
    let d = demux(&h264_aac());
    let b = play_through(&d, "/mp4/live.mp4").await;
    let bx = boxes(&b);
    assert_eq!(&bx[..2], ["ftyp", "moov"]);
    assert!(bx[2..].chunks(2).all(|c| c == ["moof", "mdat"]), "{bx:?}");
    // Four 1 s GOPs: about one fragment each (more only inside long GOPs).
    let fragments = (bx.len() - 2) / 2;
    assert!((3..=6).contains(&fragments), "{fragments} fragments");
    let probe = judge("h264", &b, 2);
    assert!(probe.contains("codec_name=h264|width=640|height=360"), "{probe}");
    assert!(probe.contains("codec_name=aac|sample_rate=48000|channels=2"), "{probe}");
}

#[tokio::test]
async fn h265_and_aac_as_fragmented_mp4() {
    if !tools() || !has_encoder("libx265") {
        eprintln!("SKIP: no libx265");
        return;
    }
    let ts = source(
        &["-c:v", "libx265", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-x265-params", "log-level=error"],
        &["-ac", "2", "-c:a", "aac"],
    );
    let d = demux(&ts);
    assert!(d.tracks.iter().any(|t| t.codec == Codec::H265));
    let b = play_through(&d, "/mp4/live.mp4").await;
    let probe = judge("h265", &b, 2);
    assert!(probe.contains("codec_name=hevc|width=640|height=360"), "{probe}");
    assert!(probe.contains("codec_name=aac"), "{probe}");
}

#[tokio::test]
async fn one_track_on_request() {
    if !tools() {
        return;
    }
    let d = demux(&h264_aac());
    let v = play_through(&d, "/mp4/live.mp4?audio=0").await;
    let probe = judge("video_only", &v, 1);
    assert!(probe.contains("codec_name=h264") && !probe.contains("aac"), "{probe}");
    let a = play_through(&d, "/mp4/live.mp4?video=0").await;
    let probe = judge("audio_only", &a, 1);
    assert!(probe.contains("codec_name=aac") && !probe.contains("h264"), "{probe}");
}

#[tokio::test]
async fn mpeg2_gets_415_pointing_to_ts() {
    if !tools() {
        return;
    }
    // What an ATSC chain carries: MPEG-2 video + MP2.
    let d = demux(&source(&["-c:v", "mpeg2video"], &["-ac", "2", "-c:a", "mp2"]));
    assert!(d.tracks.iter().any(|t| t.codec == Codec::Mpeg2Video));
    let reg = Registry::new();
    let app = caudal_mp4::router(reg.clone(), Vec::new());
    let p = reg.publish("tv", BufferConfig::default()).unwrap();
    p.set_tracks(d.tracks.clone()).unwrap();
    let res = get(&app, "/mp4/tv.mp4").await;
    assert_eq!(res.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let msg = String::from_utf8_lossy(&body(res).await).into_owned();
    assert!(msg.contains("/ts/tv.ts") && msg.contains("mpeg2"), "{msg}");
    // Leaving the video out does not help: the audio is MP2.
    let res = get(&app, "/mp4/tv.mp4?video=0").await;
    assert_eq!(res.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert!(reg.sessions().list(Some("tv")).is_empty(), "a refusal opens no session");
    drop(p);
}

fn h264_track() -> TrackInfo {
    TrackInfo {
        id: TrackId(0),
        codec: Codec::H264,
        timescale: 90_000,
        init: RawBytes::from_static(&[
            0x01, 0x64, 0x00, 0x1E, 0xFF, 0xE1, 0x00, 0x04, 0x67, 0x64, 0x00, 0x1E, 0x01, 0x00, 0x02, 0x68, 0xEE,
        ]),
        lang: None,
        video: Some(VideoParams { width: 320, height: 240, fps: None }),
        audio: None,
    }
}

fn h264_frame(i: i64, len: usize) -> Frame {
    Frame { track: TrackId(0), dts: i * 3000, pts: i * 3000, keyframe: i % 30 == 0, data: RawBytes::from(vec![0; len]) }
}

#[tokio::test]
async fn a_slow_client_is_bounded() {
    let reg = Registry::new();
    let app = caudal_mp4::router(reg.clone(), Vec::new());
    let p = reg.publish("live", BufferConfig::default()).unwrap();
    p.set_tracks(vec![h264_track()]).unwrap();
    let res = get(&app, "/mp4/live.mp4").await;
    // 120 s of video arrive before the client reads a byte.
    let n = 120 * 30;
    for i in 0..n {
        p.push(h264_frame(i, 2000)).unwrap();
        if i % 300 == 0 {
            tokio::task::yield_now().await;
        }
    }
    drop(p);
    let b = body(res).await;
    let total = n as usize * 2000;
    assert!(b.len() < total / 2, "skipped forward: {} bytes of {total}", b.len());
    assert_eq!(&boxes(&b)[..2], ["ftyp", "moov"]);
}

#[tokio::test]
async fn kicking_the_session_ends_the_body() {
    let reg = Registry::new();
    let app = caudal_mp4::router(reg.clone(), Vec::new());
    let p = reg.publish("live", BufferConfig::default()).unwrap();
    p.set_tracks(vec![h264_track()]).unwrap();
    let res = get(&app, "/mp4/live.mp4").await;
    p.push(h264_frame(0, 100)).unwrap();
    let id = reg.sessions().list(Some("live"))[0].id;
    reg.sessions().kick(id, None);
    let b = body(res).await;
    assert_eq!(boxes(&b), ["ftyp", "moov"]);
    drop(p);
}

#[tokio::test]
async fn refusals() {
    let reg = Registry::new();
    let app = caudal_mp4::router(reg.clone(), Vec::new());
    assert_eq!(get(&app, "/mp4/nobody.mp4").await.status(), StatusCode::NOT_FOUND);
    let p = reg.publish("live", BufferConfig::default()).unwrap();
    p.set_tracks(vec![h264_track()]).unwrap();
    assert_eq!(get(&app, "/mp4/live.mkv").await.status(), StatusCode::NOT_FOUND);
    assert_eq!(get(&app, "/mp4/live").await.status(), StatusCode::NOT_FOUND);
    assert_eq!(get(&app, "/mp4/.mp4").await.status(), StatusCode::NOT_FOUND);
    // Video only, and the video asked away: nothing to send.
    assert_eq!(get(&app, "/mp4/live.mp4?video=0").await.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    drop(p);
}

/// Plays only with the token `ok`.
struct TokenGate;

impl Gate for TokenGate {
    fn check<'a>(
        &'a self,
        access: Access,
        _stream: &'a str,
        token: Option<&'a str>,
        _ip: Option<std::net::IpAddr>,
    ) -> GateFuture<'a> {
        Box::pin(async move {
            match (access, token) {
                (Access::Play, None) => Err(Denied::Missing),
                (Access::Play, Some("ok")) => Ok(()),
                _ => Err(Denied::Refused("no".into())),
            }
        })
    }
}

#[tokio::test]
async fn play_tokens_apply() {
    let reg = Registry::new();
    let app = caudal_mp4::router(reg.clone(), Vec::new());
    let p = reg.publish("live", BufferConfig::default()).unwrap();
    p.set_tracks(vec![h264_track()]).unwrap();
    reg.set_gate(Arc::new(TokenGate));
    let none = get(&app, "/mp4/live.mp4").await;
    assert_eq!(none.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(none.headers()[header::WWW_AUTHENTICATE], "Bearer");
    assert_eq!(get(&app, "/mp4/live.mp4?token=bad").await.status(), StatusCode::FORBIDDEN);
    assert_eq!(get(&app, "/mp4/live.mp4?token=ok").await.status(), StatusCode::OK);
    let bearer = Request::get("/mp4/live.mp4").header("Authorization", "Bearer ok").body(Body::empty()).unwrap();
    assert_eq!(app.clone().oneshot(bearer).await.unwrap().status(), StatusCode::OK);
    assert_eq!(get(&app, "/mp4/nobody.mp4").await.status(), StatusCode::NOT_FOUND);
    drop(p);
}
