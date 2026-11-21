//! HTTP-TS output (`/ts/{name}.ts`) against real media: ffmpeg writes a
//! few seconds of MPEG-TS, caudal_ts demuxes it into tracks and frames, a
//! publisher pushes them through the registry in real time, and a plain
//! HTTP client reads the endless response. ffprobe must see the codecs and
//! `ffmpeg -xerror` must decode what came back; the body ends when the
//! stream does. A second registry pulls the output with `[[http.pull]]`.

use std::net::SocketAddr;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use caudal_core::{BufferConfig, Codec, Frame, Registry, TrackInfo};
use caudal_ts::demux::{DemuxEvent, Demuxer};
use caudal_ts::ts::TsDemux;

fn have(bin: &str) -> bool {
    Command::new(bin).arg("-version").output().is_ok_and(|o| o.status.success())
}

/// `secs` of test pattern + tone as MPEG-TS, from ffmpeg.
fn source(secs: u32, size_rate: &str, codecs: &[&str]) -> Vec<u8> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-f", "lavfi", "-i", &format!("testsrc2=size={size_rate}:duration={secs}")])
        .args(["-f", "lavfi", "-i", &format!("sine=frequency=440:sample_rate=48000:duration={secs}")])
        .args(codecs)
        .args(["-f", "mpegts", "pipe:1"])
        .output()
        .expect("run ffmpeg");
    assert!(out.status.success(), "ffmpeg: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout
}

/// Tracks and frames (in file order) of a TS file.
fn demux(ts: &[u8]) -> (Vec<TrackInfo>, Vec<Frame>) {
    let mut ts_demux = TsDemux::new();
    let mut demuxer = Demuxer::new();
    let (mut units, mut events) = (Vec::new(), Vec::new());
    for chunk in ts.chunks(1316) {
        ts_demux.feed(chunk);
        ts_demux.drain(&mut units);
    }
    ts_demux.flush(&mut units);
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
    (tracks, frames)
}

/// Serves the HTTP-TS router on a free port, with connect info as
/// `caudal`'s main does.
async fn serve(registry: Arc<Registry>) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = caudal_hls::http_ts::router(registry, Vec::new());
    tokio::spawn(async move {
        axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap();
    });
    addr
}

/// Publishes `name` and pushes `frames` paced to their own timestamps,
/// then ends the stream (the publisher drops).
fn publish_real_time(
    registry: &Arc<Registry>,
    name: &str,
    tracks: Vec<TrackInfo>,
    frames: Vec<Frame>,
) -> tokio::task::JoinHandle<()> {
    let publisher = registry.publish(name, BufferConfig::default()).expect("publish");
    publisher.set_tracks(tracks.clone()).unwrap();
    tokio::spawn(async move {
        let us = |f: &Frame| tracks.iter().find(|t| t.id == f.track).map_or(0, |t| t.to_micros(f.dts));
        let first = frames.iter().map(us).min().unwrap_or(0);
        let t0 = tokio::time::Instant::now();
        for f in frames {
            let at = Duration::from_micros((us(&f) - first).max(0) as u64);
            tokio::time::sleep_until(t0 + at).await;
            publisher.push(f).unwrap();
        }
        drop(publisher);
    })
}

/// GETs `url` on a blocking thread and reads the body to its end.
async fn read_all(url: String) -> Result<Vec<u8>, ureq::Error> {
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let resp = ureq::get(&url).call()?;
        assert_eq!(resp.headers().get("content-type").unwrap(), "video/mp2t");
        let mut out = Vec::new();
        resp.into_body().into_reader().read_to_end(&mut out)?;
        Ok(out)
    })
    .await
    .unwrap()
}

/// Writes `ts` to a file and returns ffprobe's stream list, whether the
/// first video frame is a keyframe, and whether `ffmpeg -xerror` decoded
/// all of it.
fn judge(ts: &[u8], tag: &str) -> (String, bool, Result<(), String>) {
    let path = std::env::temp_dir().join(format!("caudal_http_ts_{tag}_{}.ts", std::process::id()));
    std::fs::write(&path, ts).unwrap();
    let probe = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "stream=codec_name,width,height,sample_rate"])
        .args(["-of", "compact=p=0"])
        .arg(&path)
        .output()
        .expect("run ffprobe");
    let first_key = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-read_intervals", "%+#1"])
        .args(["-show_entries", "frame=key_frame", "-of", "csv=p=0"])
        .arg(&path)
        .output()
        .expect("run ffprobe frames");
    let first_key = String::from_utf8_lossy(&first_key.stdout).trim().starts_with('1');
    let decode = Command::new("ffmpeg")
        .args(["-v", "error", "-xerror", "-i"])
        .arg(&path)
        .args(["-f", "null", "-"])
        .output()
        .expect("run ffmpeg decode");
    let _ = std::fs::remove_file(&path);
    let decoded = if decode.status.success() && decode.stderr.is_empty() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&decode.stderr).into_owned())
    };
    (String::from_utf8_lossy(&probe.stdout).into_owned(), first_key, decoded)
}

/// PID of the TS packet at `at`.
fn pid(ts: &[u8], at: usize) -> u16 {
    (u16::from(ts[at + 1] & 0x1F) << 8) | u16::from(ts[at + 2])
}

async fn play_and_check(name: &str, src: Vec<u8>, expect: &[&str]) {
    let (tracks, frames) = demux(&src);
    assert_eq!(tracks.len(), 2, "video + audio tracks");
    let registry = Registry::new();
    let addr = serve(registry.clone()).await;
    let publishing = publish_real_time(&registry, name, tracks, frames);
    // Join mid-stream: the response starts at the newest keyframe.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let reading = tokio::spawn(read_all(format!("http://{addr}/ts/{name}.ts")));

    // While it plays, the viewer is a session with bytes counted.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let sessions = registry.sessions().list(Some(name));
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    assert_eq!(sessions[0].protocol, "http-ts");
    assert!(sessions[0].bytes_out > 0, "{sessions:?}");

    publishing.await.unwrap();
    // The stream ended: the body ends too (read_to_end returns).
    let ts = tokio::time::timeout(Duration::from_secs(10), reading)
        .await
        .expect("body ends with the stream")
        .unwrap()
        .expect("GET");
    assert!(ts.len() > 100 * 188, "{} bytes", ts.len());
    assert_eq!(ts.len() % 188, 0, "whole TS packets");
    assert!(ts.chunks(188).all(|p| p[0] == 0x47), "sync bytes");
    assert_eq!(pid(&ts, 0), 0, "PAT first");
    assert!(registry.sessions().list(Some(name)).is_empty(), "session closed");

    let (streams, first_key, decoded) = judge(&ts, name);
    assert!(first_key, "the first video frame is a keyframe");
    for e in expect {
        assert!(streams.contains(e), "{e} not in {streams}");
    }
    decoded.expect("ffmpeg -xerror decodes the output");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn h264_aac_plays_from_a_keyframe_and_ends_with_the_stream() {
    if !have("ffmpeg") || !have("ffprobe") {
        eprintln!("SKIP: ffmpeg/ffprobe not installed");
        return;
    }
    let src = source(5, "320x240:rate=25", &["-c:v", "libx264", "-preset", "veryfast", "-g", "25", "-c:a", "aac"]);
    play_and_check("cam", src, &["codec_name=h264|width=320|height=240", "codec_name=aac|sample_rate=48000"]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mpeg2_mp2_plays() {
    if !have("ffmpeg") || !have("ffprobe") {
        eprintln!("SKIP: ffmpeg/ffprobe not installed");
        return;
    }
    let src = source(
        5,
        "640x360:rate=30000/1001",
        &["-c:v", "mpeg2video", "-b:v", "4M", "-g", "15", "-bf", "2", "-c:a", "mp2", "-b:a", "128k"],
    );
    play_and_check("catv", src, &["codec_name=mpeg2video|width=640|height=360", "codec_name=mp2|sample_rate=48000"])
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_stream_and_bad_path_are_404() {
    let registry = Registry::new();
    let addr = serve(registry).await;
    for path in ["/ts/nope.ts", "/ts/nope", "/ts/.ts"] {
        match read_all(format!("http://{addr}{path}")).await {
            Err(ureq::Error::StatusCode(404)) => {}
            other => panic!("{path}: {:?}", other.map(|b| b.len())),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kicked_viewer_is_disconnected() {
    if !have("ffmpeg") {
        eprintln!("SKIP: ffmpeg not installed");
        return;
    }
    let src = source(6, "320x240:rate=25", &["-c:v", "libx264", "-preset", "veryfast", "-g", "25", "-c:a", "aac"]);
    let (tracks, frames) = demux(&src);
    let registry = Registry::new();
    let addr = serve(registry.clone()).await;
    let _publishing = publish_real_time(&registry, "k", tracks, frames);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let reading = tokio::spawn(read_all(format!("http://{addr}/ts/k.ts")));
    tokio::time::sleep(Duration::from_millis(800)).await;
    let id = registry.sessions().list(Some("k")).first().expect("a session").id;
    registry.sessions().kick(id, None);
    let body = tokio::time::timeout(Duration::from_secs(3), reading).await.expect("body ends on kick").unwrap();
    assert!(body.is_ok_and(|b| !b.is_empty()));
}

/// Caudal pulling Caudal: a second registry's `[[http.pull]]` reads this
/// output and republishes the same codecs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_pull_reads_the_output_into_another_registry() {
    if !have("ffmpeg") {
        eprintln!("SKIP: ffmpeg not installed");
        return;
    }
    let src = source(10, "320x240:rate=25", &["-c:v", "libx264", "-preset", "veryfast", "-g", "25", "-c:a", "aac"]);
    let (tracks, frames) = demux(&src);
    let origin = Registry::new();
    let addr = serve(origin.clone()).await;
    let _publishing = publish_real_time(&origin, "live", tracks, frames);

    let edge = Registry::new();
    let pulls = caudal_http_pull::start_pulls(
        edge.clone(),
        BufferConfig::default(),
        vec![caudal_http_pull::HttpPull { stream: "copy".into(), url: format!("http://{addr}/ts/live.ts") }],
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    let ok = loop {
        if let Some(s) = edge.get("copy") {
            let codecs: Vec<Codec> = s.tracks().iter().map(|t| t.codec).collect();
            if codecs.contains(&Codec::H264) && codecs.contains(&Codec::Aac) && s.stats().frames_in > 50 {
                break true;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            break false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(ok, "edge stream: {:?}", edge.get("copy").map(|s| s.tracks().iter().map(|t| t.codec).collect::<Vec<_>>()));
    assert_eq!(origin.sessions().list(Some("live")).first().map(|s| s.protocol), Some("http-ts"));
    pulls.reload(&edge, Vec::new());
}

/// A client that stops reading falls behind the stream's buffer; when it
/// reads again it resumes at a later keyframe instead of the server holding
/// everything for it. In-process (`tower::oneshot`, no socket), so the
/// kernel's socket buffers cannot absorb the stall.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stalled_client_skips_ahead_to_a_keyframe() {
    use tokio_stream::StreamExt;
    use tower::ServiceExt;
    if !have("ffmpeg") || !have("ffprobe") {
        eprintln!("SKIP: ffmpeg/ffprobe not installed");
        return;
    }
    let src = source(12, "320x240:rate=25", &["-c:v", "libx264", "-preset", "veryfast", "-g", "25", "-c:a", "aac"]);
    let (tracks, frames) = demux(&src);
    let video = tracks.iter().find(|t| t.codec == Codec::H264).unwrap().id;
    let registry = Registry::new();
    let buffer = BufferConfig { window: Duration::from_secs(1), ..BufferConfig::default() };
    let publisher = registry.publish("slow", buffer).unwrap();
    publisher.set_tracks(tracks.clone()).unwrap();
    // The first two seconds go in before the viewer joins.
    let split = frames.iter().position(|f| f.track == video && f.dts >= 2 * 90_000 && f.keyframe).unwrap();
    let mut frames = frames.into_iter();
    for f in frames.by_ref().take(split) {
        publisher.push(f).unwrap();
    }

    let app = caudal_hls::http_ts::router(registry.clone(), Vec::new());
    let req = axum::http::Request::get("/ts/slow.ts").body(axum::body::Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let mut body = resp.into_body().into_data_stream();
    let mut out = body.next().await.unwrap().unwrap().to_vec();

    // The viewer stops reading while the remaining 10 s arrive at once,
    // ten times the buffer window; then the stream ends.
    for f in frames {
        publisher.push(f).unwrap();
    }
    drop(publisher);
    tokio::time::sleep(Duration::from_millis(200)).await;
    while let Some(chunk) = tokio::time::timeout(Duration::from_secs(5), body.next()).await.expect("body ends") {
        out.extend_from_slice(&chunk.unwrap());
    }
    assert!(out.len() < src.len() / 2, "skipped ahead: {} of {} bytes", out.len(), src.len());

    let path = std::env::temp_dir().join(format!("caudal_http_ts_stall_{}.ts", std::process::id()));
    std::fs::write(&path, &out).unwrap();
    let packets = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "packet=dts_time,flags", "-of", "csv=p=0"])
        .arg(&path)
        .output()
        .expect("run ffprobe packets");
    let (_, _, decoded) = judge(&out, "stall");
    let _ = std::fs::remove_file(&path);
    let packets = String::from_utf8_lossy(&packets.stdout).into_owned();
    // (dts seconds, keyframe) per video packet.
    let v: Vec<(f64, bool)> = packets
        .lines()
        .filter_map(|l| {
            let mut it = l.split(',');
            Some((it.next()?.parse().ok()?, it.next()?.starts_with('K')))
        })
        .collect();
    // Frames are 40 ms apart; a gap of more than a few means frames were
    // skipped. Depending on scheduling the viewer skips once or once per
    // GOP; either way every resume point is a keyframe.
    let gaps: Vec<&[(f64, bool)]> = v.windows(2).filter(|w| w[1].0 - w[0].0 > 0.2).collect();
    assert!(!gaps.is_empty(), "frames were skipped: {packets}");
    assert!(gaps.iter().all(|w| w[1].1), "every resume is a keyframe: {packets}");
    decoded.expect("ffmpeg -xerror decodes across the jump");
}
