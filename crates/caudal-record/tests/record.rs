//! Recorder tests: a real `Registry` fed with real H.264 + AAC frames from a
//! fixture ffmpeg wrote (256x144, 30 fps, GOP 60 = 2 s, 48 kHz AAC,
//! B-frames on), recorded into a temp dir; the VOD and clips are checked
//! with ffprobe / ffmpeg over a real local HTTP server.

#[path = "support/mp4demux.rs"]
mod mp4demux;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use caudal_core::{BufferConfig, Publisher, Registry, TrackId};
use caudal_record::{RecordConfig, RecordService};
use mp4_atom::{Any, Decode};
use mp4demux::{Demuxed, demux};
use tower::ServiceExt;

fn fixture() -> Demuxed {
    demux(include_bytes!("fixtures/av.mp4"))
}

fn cfg(dir: &Path) -> RecordConfig {
    RecordConfig {
        dir: dir.to_owned(),
        streams: vec!["*".into()],
        segment_secs: 4,
        retention_hours: None,
        upload_url: None,
    }
}

struct Reply {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: bytes::Bytes,
}

async fn call(app: &Router, method: &str, path: &str, body: &str) -> Reply {
    let req = Request::builder().method(method).uri(path).body(Body::from(body.to_owned())).unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    Reply { status, headers, body }
}

async fn publish(reg: &Arc<Registry>, name: &str, fx: &Demuxed) -> Publisher {
    let p = reg.publish(name, BufferConfig::default()).unwrap();
    p.set_tracks(fx.tracks.clone()).unwrap();
    p
}

/// Pushes `loops` loops of the fixture, a GOP at a time with a short pause,
/// so the recorder runs alongside the publisher.
async fn push(p: &Publisher, fx: &Demuxed, loops: std::ops::Range<i64>) {
    for n in loops {
        for (i, f) in fx.looped(n).enumerate() {
            p.push(f).unwrap();
            if i % 60 == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    }
}

async fn recordings(app: &Router) -> Vec<serde_json::Value> {
    let r = call(app, "GET", "/api/v1/recordings", "").await;
    assert_eq!(r.status, StatusCode::OK);
    serde_json::from_slice(&r.body).unwrap()
}

/// Waits until the list holds `n` recordings, all ended.
async fn wait_ended(app: &Router, n: usize) -> Vec<serde_json::Value> {
    let t0 = Instant::now();
    loop {
        let list = recordings(app).await;
        if list.len() == n && list.iter().all(|m| !m["ended_at"].is_null()) {
            return list;
        }
        assert!(t0.elapsed() < Duration::from_secs(20), "recordings never ended: {list:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn serve(app: Router) -> SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    addr
}

/// Runs a tool with a hard timeout; the child is killed if the test stops.
async fn run(cmd: &str, args: &[&str]) -> std::process::Output {
    let child = tokio::process::Command::new(cmd)
        .args(args)
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(60), child.wait_with_output()).await.expect("tool timed out").unwrap()
}

fn ffprobe_json(out: &std::process::Output) -> serde_json::Value {
    assert!(out.status.success(), "ffprobe failed: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

/// First sample of track 1 in every fragment of a segment: is the first one a sync sample?
fn starts_with_keyframe(data: &[u8]) -> bool {
    let mut buf = data;
    while !buf.is_empty() {
        if let Any::Moof(m) = Any::decode(&mut buf).unwrap() {
            let traf = m.traf.iter().find(|t| t.tfhd.track_id == 1).unwrap();
            let flags = traf.trun[0].entries[0].flags.unwrap();
            return flags & 0x0001_0000 == 0;
        }
    }
    false
}

fn files(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> =
        std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    v.sort();
    v
}

#[tokio::test]
async fn records_plays_as_vod_and_clips() {
    let tmp = tempfile::tempdir().unwrap();
    let reg = Registry::new();
    let svc = caudal_record::start(reg.clone(), cfg(tmp.path()), Vec::new()).unwrap();
    let app = svc.router();
    let fx = fixture();

    let p = publish(&reg, "cam1", &fx).await;
    push(&p, &fx, 0..5).await; // 20 s of media

    // While live: listed, not ended, not deletable, EVENT playlist.
    let t0 = Instant::now();
    let live = loop {
        let l = recordings(&app).await;
        if l.len() == 1 && l[0]["segments"].as_u64().unwrap_or(0) >= 2 {
            break l;
        }
        assert!(t0.elapsed() < Duration::from_secs(20), "{l:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(live[0]["ended_at"].is_null());
    let id = live[0]["id"].as_str().unwrap().to_owned();
    assert!(id.len() == 16 && id.ends_with('Z'), "{id}");
    let del = call(&app, "DELETE", &format!("/api/v1/recordings/cam1/{id}"), "").await;
    assert_eq!(del.status, StatusCode::CONFLICT);
    let pl = call(&app, "GET", &format!("/vod/cam1/{id}/index.m3u8"), "").await;
    assert_eq!(pl.status, StatusCode::OK);
    assert_eq!(pl.headers["content-type"], "application/vnd.apple.mpegurl");
    let text = String::from_utf8_lossy(&pl.body).into_owned();
    assert!(text.contains("#EXT-X-PLAYLIST-TYPE:EVENT") && !text.contains("ENDLIST"), "{text}");

    drop(p);
    let list = wait_ended(&app, 1).await;
    let m = &list[0];
    assert_eq!(m["stream"], "cam1");
    assert!(m["error"].is_null(), "{m}");
    let dur = m["duration_ms"].as_u64().unwrap();
    assert!((19_000..=21_000).contains(&dur), "duration_ms {dur}");
    let tracks = m["tracks"].as_array().unwrap();
    assert_eq!(tracks[0]["kind"], "video");
    assert_eq!(tracks[0]["codec"], "h264");
    assert_eq!(tracks[0]["width"], 256);
    assert_eq!(tracks[1]["codec"], "aac");
    assert_eq!(tracks[1]["sample_rate"], 48_000);

    // Files on disk: init, playlist, meta, complete segments that each
    // start with a keyframe, no temp files.
    let dir = tmp.path().join("cam1").join(&id);
    let names = files(&dir);
    assert!(names.iter().all(|n| !n.ends_with(".tmp")), "{names:?}");
    let segs: Vec<&String> = names.iter().filter(|n| n.starts_with("seg-")).collect();
    assert_eq!(segs.len() as u64, m["segments"].as_u64().unwrap());
    assert!(segs.len() >= 4, "{names:?}");
    for s in &segs {
        assert!(starts_with_keyframe(&std::fs::read(dir.join(s)).unwrap()), "{s} does not start with a keyframe");
    }
    let text = std::fs::read_to_string(dir.join("index.m3u8")).unwrap();
    for needle in [
        "#EXT-X-VERSION:7",
        "#EXT-X-PLAYLIST-TYPE:VOD",
        "#EXT-X-ENDLIST",
        "#EXT-X-MAP:URI=\"init.mp4\"",
        "#EXT-X-PROGRAM-DATE-TIME:",
        "#EXT-X-TARGETDURATION:4",
        "seg-000001.m4s",
    ] {
        assert!(text.contains(needle), "{needle} missing:\n{text}");
    }

    let addr = serve(app.clone()).await;
    let url = format!("http://{addr}/vod/cam1/{id}/index.m3u8");

    // ffprobe: h264 + aac, duration within 10% of the 20 s source.
    let out = run(
        "ffprobe",
        &["-v", "error", "-hide_banner", "-show_entries", "format=duration:stream=codec_name", "-of", "json", &url],
    )
    .await;
    assert!(out.stderr.is_empty(), "ffprobe errors: {}", String::from_utf8_lossy(&out.stderr));
    let j = ffprobe_json(&out);
    let codecs: Vec<&str> =
        j["streams"].as_array().unwrap().iter().map(|s| s["codec_name"].as_str().unwrap()).collect();
    assert!(codecs.contains(&"h264") && codecs.contains(&"aac"), "{codecs:?}");
    let d: f64 = j["format"]["duration"].as_str().unwrap().parse().unwrap();
    eprintln!("vod duration {d}");
    assert!((18.0..=22.0).contains(&d), "vod duration {d}");

    // ffmpeg decodes the whole VOD cleanly.
    let out = run("ffmpeg", &["-hide_banner", "-v", "error", "-i", &url, "-f", "null", "-"]).await;
    assert!(out.status.success() && out.stderr.is_empty(), "decode errors: {}", String::from_utf8_lossy(&out.stderr));

    // Clip [2000, 5000): a progressive MP4 of about 3 s (± one GOP).
    let body = format!(r#"{{"stream":"cam1","id":"{id}","from_ms":2000,"to_ms":5000}}"#);
    let clip = call(&app, "POST", "/api/v1/clips", &body).await;
    assert_eq!(clip.status, StatusCode::OK, "{}", String::from_utf8_lossy(&clip.body));
    assert_eq!(clip.headers["content-type"], "video/mp4");
    assert_eq!(
        clip.headers["content-disposition"].to_str().unwrap(),
        format!("attachment; filename=\"cam1-{id}-2000-5000.mp4\"")
    );
    assert_eq!(clip.headers["content-length"].to_str().unwrap(), clip.body.len().to_string());
    assert_eq!(&clip.body[4..8], b"ftyp");
    let clip_path = tmp.path().join("clip.mp4");
    std::fs::write(&clip_path, &clip.body).unwrap();
    let cp = clip_path.to_str().unwrap();
    let out = run(
        "ffprobe",
        &["-v", "error", "-hide_banner", "-show_entries", "format=duration:stream=codec_name", "-of", "json", cp],
    )
    .await;
    assert!(out.stderr.is_empty(), "clip ffprobe errors: {}", String::from_utf8_lossy(&out.stderr));
    let j = ffprobe_json(&out);
    let d: f64 = j["format"]["duration"].as_str().unwrap().parse().unwrap();
    eprintln!("clip duration {d}");
    assert!((1.0..=5.0).contains(&d) && (d - 3.0).abs() <= 2.0, "clip duration {d}");
    assert_eq!(j["streams"].as_array().unwrap().len(), 2, "{j}");
    let out = run("ffmpeg", &["-hide_banner", "-v", "error", "-i", cp, "-f", "null", "-"]).await;
    assert!(out.status.success() && out.stderr.is_empty(), "clip decode: {}", String::from_utf8_lossy(&out.stderr));

    // Bad clip ranges.
    for (from, to) in [(5000, 2000), (-1, 1000), (100_000, 200_000), (1000, 1000)] {
        let body = format!(r#"{{"stream":"cam1","id":"{id}","from_ms":{from},"to_ms":{to}}}"#);
        assert_eq!(call(&app, "POST", "/api/v1/clips", &body).await.status, StatusCode::BAD_REQUEST, "{from}..{to}");
    }
    assert_eq!(call(&app, "POST", "/api/v1/clips", "not json").await.status, StatusCode::BAD_REQUEST);
    let body = r#"{"stream":"cam1","id":"20000101T000000Z","from_ms":0,"to_ms":1000}"#;
    assert_eq!(call(&app, "POST", "/api/v1/clips", body).await.status, StatusCode::NOT_FOUND);

    // VOD files and content types.
    let r = call(&app, "GET", &format!("/vod/cam1/{id}/init.mp4"), "").await;
    assert_eq!((r.status, r.headers["content-type"].to_str().unwrap()), (StatusCode::OK, "video/mp4"));
    let r = call(&app, "GET", &format!("/vod/cam1/{id}/seg-000001.m4s"), "").await;
    assert_eq!((r.status, r.headers["content-type"].to_str().unwrap()), (StatusCode::OK, "video/iso.segment"));
    let r = call(&app, "GET", &format!("/api/v1/recordings/cam1/{id}"), "").await;
    assert_eq!(r.status, StatusCode::OK);

    // Path traversal and junk names.
    for path in [
        format!("/vod/cam1/{id}/meta.json"),
        format!("/vod/cam1/{id}/..%2F..%2Fetc%2Fpasswd"),
        format!("/vod/cam1/{id}/seg-000001.m4s.tmp"),
        format!("/vod/..%2F..%2Fetc/{id}/init.mp4"),
        "/vod/cam1/..%2F..%2F/init.mp4".into(),
        "/vod/cam1/../init.mp4".into(),
        format!("/vod/%2E%2E/{id}/index.m3u8"),
        "/api/v1/recordings/cam1/..".into(),
        "/api/v1/recordings/.hidden/20260918T000000Z".into(),
    ] {
        let s = call(&app, "GET", &path, "").await.status;
        assert!(s == StatusCode::BAD_REQUEST || s == StatusCode::NOT_FOUND, "{path} -> {s}");
    }

    // Delete after the end.
    let del = call(&app, "DELETE", &format!("/api/v1/recordings/cam1/{id}"), "").await;
    assert_eq!(del.status, StatusCode::NO_CONTENT);
    assert!(!dir.exists());
    assert_eq!(call(&app, "GET", &format!("/api/v1/recordings/cam1/{id}"), "").await.status, StatusCode::NOT_FOUND);
    assert!(recordings(&app).await.is_empty());
    drop(svc);
}

#[tokio::test]
async fn retention_deletes_old_ended_recordings_only() {
    let tmp = tempfile::tempdir().unwrap();
    let reg = Registry::new();
    let svc =
        caudal_record::start(reg.clone(), RecordConfig { retention_hours: Some(1), ..cfg(tmp.path()) }, Vec::new())
            .unwrap();
    let app = svc.router();
    let fx = fixture();

    let p = publish(&reg, "old", &fx).await;
    push(&p, &fx, 0..2).await;
    drop(p);
    let list = wait_ended(&app, 1).await;
    let old_id = list[0]["id"].as_str().unwrap().to_owned();

    // A second one still recording.
    let live = publish(&reg, "live", &fx).await;
    push(&live, &fx, 0..2).await;
    let t0 = Instant::now();
    let live_id = loop {
        let l = recordings(&app).await;
        if let Some(m) = l.iter().find(|m| m["stream"] == "live") {
            break m["id"].as_str().unwrap().to_owned();
        }
        assert!(t0.elapsed() < Duration::from_secs(10));
        tokio::time::sleep(Duration::from_millis(20)).await;
    };

    // A fresh ended recording is kept.
    assert_eq!(svc.sweep().await, 0);

    // Age both: the ended one goes, the one being written stays.
    for (stream, id) in [("old", &old_id), ("live", &live_id)] {
        let path = tmp.path().join(stream).join(id).join("meta.json");
        let mut m: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        m["ended_at"] = "2020-01-01T00:00:00Z".into();
        std::fs::write(&path, serde_json::to_vec(&m).unwrap()).unwrap();
    }
    assert_eq!(svc.sweep().await, 1);
    assert!(!tmp.path().join("old").join(&old_id).exists());
    assert!(tmp.path().join("live").join(&live_id).exists());
    drop(live);
}

#[tokio::test]
async fn uploads_mirror_to_file_url() {
    let tmp = tempfile::tempdir().unwrap();
    let bucket = tempfile::tempdir().unwrap();
    let url = format!("file://{}/rec", bucket.path().display());
    let reg = Registry::new();
    let svc = caudal_record::start(reg.clone(), RecordConfig { upload_url: Some(url), ..cfg(tmp.path()) }, Vec::new())
        .unwrap();
    let app = svc.router();
    let fx = fixture();

    let p = publish(&reg, "up", &fx).await;
    push(&p, &fx, 0..3).await;
    drop(p);
    let list = wait_ended(&app, 1).await;
    let id = list[0]["id"].as_str().unwrap().to_owned();
    let local = tmp.path().join("up").join(&id);
    let remote = bucket.path().join("rec").join("up").join(&id);

    let t0 = Instant::now();
    loop {
        let same = svc.uploads_pending() == 0
            && remote.exists()
            && files(&local) == files(&remote)
            && files(&local)
                .iter()
                .all(|f| std::fs::read(local.join(f)).unwrap() == std::fs::read(remote.join(f)).unwrap());
        if same {
            break;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(20),
            "not mirrored: {:?} vs {:?}",
            files(&local),
            remote.exists().then(|| files(&remote))
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(files(&remote).contains(&"seg-000001.m4s".to_string()));
}

#[tokio::test]
async fn track_change_starts_a_new_recording() {
    let tmp = tempfile::tempdir().unwrap();
    let reg = Registry::new();
    let svc = caudal_record::start(reg.clone(), cfg(tmp.path()), Vec::new()).unwrap();
    let app = svc.router();
    let fx = fixture();

    let p = publish(&reg, "tc", &fx).await;
    push(&p, &fx, 0..2).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Video only from here on: a new init, so a new recording.
    let video: Vec<_> = fx.tracks.iter().filter(|t| t.id == TrackId(0)).cloned().collect();
    p.set_tracks(video).unwrap();
    for n in 2..4 {
        for f in fx.looped(n).filter(|f| f.track == TrackId(0)) {
            p.push(f).unwrap();
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    drop(p);
    let list = wait_ended(&app, 2).await;
    let kinds: Vec<usize> = list.iter().map(|m| m["tracks"].as_array().unwrap().len()).collect();
    // Newest first: the video-only one.
    assert_eq!(kinds, vec![1, 2], "{list:?}");
    assert!(list.iter().all(|m| m["segments"].as_u64().unwrap() >= 1));
    let ids: Vec<&str> = list.iter().map(|m| m["id"].as_str().unwrap()).collect();
    assert_ne!(ids[0], ids[1]);
}

#[tokio::test]
async fn unmatched_streams_are_not_recorded_and_restart_closes_interrupted() {
    let tmp = tempfile::tempdir().unwrap();
    // An interrupted recording left by a crash: meta without ended_at, a
    // half-written segment, an EVENT playlist.
    let crashed = tmp.path().join("cam").join("20260101T000000Z");
    std::fs::create_dir_all(&crashed).unwrap();
    std::fs::write(crashed.join("seg-000002.m4s.tmp"), b"half").unwrap();
    std::fs::write(
        crashed.join("index.m3u8"),
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:4\n#EXT-X-PLAYLIST-TYPE:EVENT\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\nseg-000001.m4s\n",
    )
    .unwrap();
    std::fs::write(
        crashed.join("meta.json"),
        r#"{"stream":"cam","id":"20260101T000000Z","started_at":"2026-01-01T00:00:00.000Z","ended_at":null,"duration_ms":4000,"bytes":1,"segments":1,"tracks":[]}"#,
    )
    .unwrap();

    let reg = Registry::new();
    let svc =
        caudal_record::start(reg.clone(), RecordConfig { streams: vec!["cam*".into()], ..cfg(tmp.path()) }, Vec::new())
            .unwrap();
    let app: Router = svc.router();
    assert!(!crashed.join("seg-000002.m4s.tmp").exists());
    let pl = std::fs::read_to_string(crashed.join("index.m3u8")).unwrap();
    assert!(pl.contains("#EXT-X-PLAYLIST-TYPE:VOD") && pl.ends_with("#EXT-X-ENDLIST\n"), "{pl}");
    let list = recordings(&app).await;
    assert_eq!(list.len(), 1);
    assert!(!list[0]["ended_at"].is_null() && !list[0]["error"].is_null(), "{list:?}");
    // Now deletable.
    assert_eq!(
        call(&app, "DELETE", "/api/v1/recordings/cam/20260101T000000Z", "").await.status,
        StatusCode::NO_CONTENT
    );

    let fx = fixture();
    let p = publish(&reg, "other", &fx).await;
    push(&p, &fx, 0..1).await;
    drop(p);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!tmp.path().join("other").exists());
    let _: &RecordService = &svc;
    let _: PathBuf = tmp.path().to_owned();
}
