//! `/audio/{name}.aac|.mp3` through the router: real AAC and MP2 from
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
    Access, AudioParams, BufferConfig, Codec, Denied, Frame, Gate, GateFuture, Registry, TrackId, TrackInfo,
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

/// Three seconds of MPEG-TS from ffmpeg: a sine, plus test video if asked.
fn source(audio_args: &[&str], video: bool) -> Vec<u8> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-v", "error"]);
    if video {
        cmd.args(["-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30:duration=3"]);
    }
    cmd.args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=3"]);
    if video {
        cmd.args(["-c:v", "mpeg2video", "-g", "15"]);
    }
    let out = cmd.args(audio_args).args(["-f", "mpegts", "pipe:1"]).output().expect("run ffmpeg");
    assert!(out.status.success(), "ffmpeg: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout
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

fn ffprobe(path: &std::path::Path) -> String {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "stream=codec_name,sample_rate,channels", "-of", "compact=p=0"])
        .arg(path)
        .output()
        .expect("run ffprobe");
    String::from_utf8_lossy(&out.stdout).into_owned()
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

/// Publishes `d` as `radio`, opens `path` before the first frame, pushes
/// everything, ends the stream, and returns the whole response body.
async fn play_through(d: &Demuxed, path: &str, content_type: &str) -> Bytes {
    let reg = Registry::new();
    let app = caudal_audio::router(reg.clone(), Vec::new());
    let p = reg.publish("radio", BufferConfig::default()).unwrap();
    p.set_tracks(d.tracks.clone()).unwrap();
    let res = get(&app, path).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CONTENT_TYPE], content_type);
    assert!(res.headers().get(header::CONTENT_LENGTH).is_none(), "endless: no length");
    let sessions = reg.sessions().list(Some("radio"));
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].protocol, "audio");
    assert_eq!(reg.get("radio").unwrap().stats().viewers, 1);
    for f in &d.frames {
        p.push(f.clone()).unwrap();
    }
    drop(p);
    let b = body(res).await;
    assert_eq!(reg.sessions().list(Some("radio")).len(), 0, "session closes with the body");
    b
}

fn write_tmp(tag: &str, ext: &str, data: &[u8]) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("caudal_audio_{tag}_{}.{ext}", std::process::id()));
    std::fs::write(&path, data).unwrap();
    path
}

#[tokio::test]
async fn aac_passes_through_as_adts() {
    if !tools() {
        return;
    }
    let d = demux(&source(&["-ac", "2", "-c:a", "aac", "-b:a", "128k"], false));
    let audio = d.tracks.iter().find(|t| t.codec == Codec::Aac).expect("an AAC track");
    assert_eq!(audio.audio, Some(AudioParams { sample_rate: 48_000, channels: 2 }));
    let frames: usize = d.frames.iter().map(|f| f.data.len() + 7).sum();

    let b = play_through(&d, "/audio/radio.aac", "audio/aac").await;
    assert_eq!(b.len(), frames, "every frame, each with a 7-byte ADTS header");
    let path = write_tmp("aac", "aac", &b);
    let probe = ffprobe(&path);
    let decoded = decodes(&path);
    let _ = std::fs::remove_file(&path);
    assert!(probe.contains("codec_name=aac|sample_rate=48000|channels=2"), "{probe}");
    decoded.expect("decodes without an error");
}

#[tokio::test]
async fn mp2_from_a_broadcast_feed_passes_through_as_mpeg_audio() {
    if !tools() {
        return;
    }
    // What an ATSC chain carries: MPEG-2 video + MP2 (stereo, 48 kHz).
    let d = demux(&source(&["-ac", "2", "-c:a", "mp2", "-b:a", "192k"], true));
    assert!(d.tracks.iter().any(|t| t.codec == Codec::Mpeg2Video));
    let audio = d.tracks.iter().find(|t| t.codec == Codec::Mp2).expect("an MP2 track").clone();

    let b = play_through(&d, "/audio/radio.mp3", "audio/mpeg").await;
    // The listener joins at the first video keyframe (the stream's first
    // join point), so audio before it is not sent; nothing after it is lost.
    let first_key = d.frames.iter().position(|f| f.keyframe && f.track != audio.id).unwrap();
    let expect: usize = d.frames[first_key..].iter().filter(|f| f.track == audio.id).map(|f| f.data.len()).sum();
    assert_eq!(b.len(), expect, "audio frames only, untouched");
    let path = write_tmp("mp2", "mp3", &b);
    let probe = ffprobe(&path);
    let decoded = decodes(&path);
    let _ = std::fs::remove_file(&path);
    assert!(probe.contains("codec_name=mp2|sample_rate=48000|channels=2"), "{probe}");
    decoded.expect("decodes without an error");
}

fn aac_track() -> TrackInfo {
    TrackInfo {
        id: TrackId(0),
        codec: Codec::Aac,
        timescale: 48_000,
        init: RawBytes::from_static(&[0x11, 0x90]),
        lang: None,
        video: None,
        audio: Some(AudioParams { sample_rate: 48_000, channels: 2 }),
    }
}

fn aac_frame(i: i64) -> Frame {
    aac_frame_of(i, 10)
}

fn aac_frame_of(i: i64, len: usize) -> Frame {
    Frame { track: TrackId(0), dts: i * 1024, pts: i * 1024, keyframe: true, data: RawBytes::from(vec![0x21; len]) }
}

#[tokio::test]
async fn a_slow_listener_resyncs_to_live() {
    let reg = Registry::new();
    let app = caudal_audio::router(reg.clone(), Vec::new());
    let p = reg.publish("radio", BufferConfig::default()).unwrap();
    p.set_tracks(vec![aac_track()]).unwrap();
    let res = get(&app, "/audio/radio.aac").await;
    // 20 s of audio arrive before the listener reads a byte.
    let n = 20 * 48_000 / 1024;
    for i in 0..n {
        p.push(aac_frame(i)).unwrap();
    }
    drop(p);
    let b = body(res).await;
    assert!(!b.is_empty());
    assert!(b.len() < 17 * (n as usize) / 4, "skipped to live: {} bytes of {}", b.len(), 17 * n);
}

#[tokio::test]
async fn kicking_the_session_ends_the_body() {
    let reg = Registry::new();
    let app = caudal_audio::router(reg.clone(), Vec::new());
    let p = reg.publish("radio", BufferConfig::default()).unwrap();
    p.set_tracks(vec![aac_track()]).unwrap();
    let res = get(&app, "/audio/radio.aac").await;
    p.push(aac_frame(0)).unwrap();
    let id = reg.sessions().list(Some("radio"))[0].id;
    reg.sessions().kick(id, None);
    let b = body(res).await;
    assert!(b.len() <= 17);
    drop(p);
}

#[tokio::test]
async fn icy_metadata_on_request() {
    let reg = Registry::new();
    let app = caudal_audio::router(reg.clone(), Vec::new());
    let p = reg.publish("oceano", BufferConfig::default()).unwrap();
    p.set_tracks(vec![aac_track()]).unwrap();
    let req = Request::get("/audio/oceano.aac").header("Icy-MetaData", "1").body(Body::empty()).unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.headers()["icy-metaint"], caudal_audio::ICY_METAINT.to_string().as_str());
    assert_eq!(res.headers()["icy-name"], "oceano");
    // Big frames, so a metadata interval fits well inside `MAX_BEHIND`.
    let frames = caudal_audio::ICY_METAINT / 407 + 10;
    for i in 0..frames as i64 {
        p.push(aac_frame_of(i, 400)).unwrap();
    }
    drop(p);
    let b = body(res).await;
    let at = caudal_audio::ICY_METAINT;
    let title = b"StreamTitle='oceano';";
    assert_eq!(&b[at + 1..at + 1 + title.len()], title);
    assert_eq!(b.len(), frames * 407 + 1 + usize::from(b[at]) * 16);
}

#[tokio::test]
async fn refusals() {
    let reg = Registry::new();
    let app = caudal_audio::router(reg.clone(), Vec::new());
    assert_eq!(get(&app, "/audio/nobody.aac").await.status(), StatusCode::NOT_FOUND);
    let p = reg.publish("radio", BufferConfig::default()).unwrap();
    p.set_tracks(vec![aac_track()]).unwrap();
    assert_eq!(get(&app, "/audio/radio.wav").await.status(), StatusCode::NOT_FOUND);
    assert_eq!(get(&app, "/audio/radio").await.status(), StatusCode::NOT_FOUND);
    let wrong = get(&app, "/audio/radio.mp3").await;
    assert_eq!(wrong.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let msg = String::from_utf8_lossy(&body(wrong).await).into_owned();
    assert!(msg.contains("/audio/radio.aac"), "{msg}");

    // No audio track at all.
    let v = reg.publish("tv", BufferConfig::default()).unwrap();
    let mut video = aac_track();
    video.codec = Codec::H264;
    video.audio = None;
    v.set_tracks(vec![video]).unwrap();
    assert_eq!(get(&app, "/audio/tv.aac").await.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    drop((p, v));
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
    let app = caudal_audio::router(reg.clone(), Vec::new());
    let p = reg.publish("radio", BufferConfig::default()).unwrap();
    p.set_tracks(vec![aac_track()]).unwrap();
    reg.set_gate(Arc::new(TokenGate));
    let none = get(&app, "/audio/radio.aac").await;
    assert_eq!(none.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(none.headers()[header::WWW_AUTHENTICATE], "Bearer");
    assert_eq!(get(&app, "/audio/radio.aac?token=bad").await.status(), StatusCode::FORBIDDEN);
    assert_eq!(get(&app, "/audio/radio.aac?token=ok").await.status(), StatusCode::OK);
    let bearer = Request::get("/audio/radio.aac").header("Authorization", "Bearer ok").body(Body::empty()).unwrap();
    assert_eq!(app.clone().oneshot(bearer).await.unwrap().status(), StatusCode::OK);
    // An unknown name stays a 404 whatever the gate says.
    assert_eq!(get(&app, "/audio/nobody.aac").await.status(), StatusCode::NOT_FOUND);
    drop(p);
}
