//! End-to-end: the whole path a real user takes, against the real binary.
//!
//!   ffmpeg (RTMP publish) → caudal → LL-HLS playlist + parts → validator
//!
//! Runs only with `CAUDAL_E2E=1` and ffmpeg on PATH, so unit test runs stay
//! fast. It is red until batch 1 lands; that is the point. Batch 1 is not
//! done while this is red.

mod support;

use std::time::{Duration, Instant};

use support::{Publisher, Server, have};

fn enabled() -> bool {
    if std::env::var("CAUDAL_E2E").is_err() {
        eprintln!("SKIP: set CAUDAL_E2E=1 to run end-to-end tests");
        return false;
    }
    if !have("ffmpeg") {
        eprintln!("SKIP: ffmpeg not on PATH");
        return false;
    }
    true
}

#[test]
fn shell_answers_health_and_lists_no_streams() {
    if !enabled() {
        return;
    }
    let s = Server::start();
    assert_eq!(s.get("/healthz").unwrap().0, 200);
    assert_eq!(s.get("/readyz").unwrap().0, 200);
    let (code, body) = s.get("/api/v1/streams").unwrap();
    assert_eq!(code, 200);
    assert_eq!(body.trim(), "[]", "no streams yet");
    let (code, body) = s.get("/metrics").unwrap();
    assert_eq!(code, 200);
    assert!(body.contains("caudal_"), "prometheus exposition has our metrics");
}

#[test]
fn rtmp_publish_appears_in_api_and_ends_on_disconnect() {
    if !enabled() {
        return;
    }
    let s = Server::start();
    let publ = Publisher::rtmp(&s.rtmp_url("e2e"), 30);

    let body = s.wait_until("/api/v1/streams/e2e", Duration::from_secs(15), |b| {
        b.contains("\"h264\"") && b.contains("\"aac\"")
    });
    assert!(body.contains("\"name\":\"e2e\""), "{body}");

    // frames_in must keep rising while ffmpeg publishes.
    let read = |b: &str| -> u64 {
        b.split("\"frames_in\":").nth(1).and_then(|t| t.split(|c: char| !c.is_ascii_digit()).next()).and_then(|n| n.parse().ok()).unwrap_or(0)
    };
    let a = read(&s.get("/api/v1/streams/e2e").unwrap().1);
    std::thread::sleep(Duration::from_secs(2));
    let b = read(&s.get("/api/v1/streams/e2e").unwrap().1);
    assert!(b > a + 30, "frames_in should rise ~60/s, got {a} -> {b}");

    drop(publ);
    let t0 = Instant::now();
    loop {
        if s.get("/api/v1/streams/e2e").unwrap().0 == 404 {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(5), "stream must end within 5 s of disconnect");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn wrong_app_name_is_rejected() {
    if !enabled() {
        return;
    }
    let s = Server::start();
    let url = format!("rtmp://127.0.0.1:{}/wrongapp/e2e", s.rtmp);
    let _publ = Publisher::rtmp(&url, 5);
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(s.get("/api/v1/streams/e2e").unwrap().0, 404);
}

#[test]
fn ll_hls_plays_and_validates() {
    if !enabled() {
        return;
    }
    let s = Server::start();
    let _publ = Publisher::rtmp(&s.rtmp_url("e2e"), 60);

    let playlist = s.wait_until("/hls/e2e/index.m3u8", Duration::from_secs(20), |b| b.contains("#EXT-X-PART"));
    for tag in ["#EXTM3U", "#EXT-X-VERSION", "#EXT-X-SERVER-CONTROL", "#EXT-X-PART-INF", "#EXT-X-PART", "#EXT-X-MAP", "#EXT-X-PROGRAM-DATE-TIME"] {
        assert!(playlist.contains(tag), "playlist lacks {tag}:\n{playlist}");
    }
    assert!(playlist.contains("CAN-BLOCK-RELOAD=YES"), "blocking reload is on by default");

    // Init segment and the newest part must be fetchable fMP4.
    let init = s.get_bytes("/hls/e2e/init.mp4");
    assert_eq!(&init[4..8], b"ftyp", "init.mp4 starts with ftyp");
    let part = playlist.lines().rev().find_map(|l| l.strip_prefix("#EXT-X-PART:")).and_then(|l| l.split("URI=\"").nth(1)).and_then(|u| u.split('"').next()).expect("a part URI");
    let bytes = s.get_bytes(&format!("/hls/e2e/{part}"));
    assert!(bytes.windows(4).any(|w| w == b"moof"), "part is a CMAF fragment");

    // Blocking reload: asking for the next part must wait, not 404.
    let msn: u64 = playlist.lines().find_map(|l| l.strip_prefix("#EXT-X-MEDIA-SEQUENCE:")).unwrap().trim().parse().unwrap();
    let segs = playlist.lines().filter(|l| l.starts_with("#EXTINF")).count() as u64;
    let t0 = Instant::now();
    let (code, _) = s.get(&format!("/hls/e2e/index.m3u8?_HLS_msn={}&_HLS_part=0", msn + segs)).unwrap();
    assert_eq!(code, 200);
    assert!(t0.elapsed() >= Duration::from_millis(100), "a future part should block until it exists");

    // A viewer joining now must have a join point within 2 s of playlist age.
    let joined = s.wait_until("/hls/e2e/index.m3u8", Duration::from_secs(5), |b| b.contains("#EXT-X-PART"));
    assert!(joined.contains("INDEPENDENT=YES"), "at least one independent part to join on");

    // Apple's validator, when installed (macOS with Xcode tools).
    let dir = tempfile::tempdir().unwrap();
    match support::validate_hls(&s.url("/hls/e2e/index.m3u8"), dir.path()) {
        Some(Ok(())) => {}
        Some(Err(report)) => panic!("mediastreamvalidator reported errors:\n{report}"),
        None => eprintln!("NOT VERIFIED: mediastreamvalidator not installed; LL-HLS conformance unchecked on this machine"),
    }

    // The player page exists and carries the latency readout.
    let (code, page) = s.get("/play/e2e").unwrap();
    assert_eq!(code, 200);
    assert!(page.contains("hls.js") || page.contains("Hls("), "page uses hls.js");
    assert!(page.contains("latency"), "page shows measured latency");
}

#[test]
fn config_check_rejects_unknown_key_with_line_number() {
    if !enabled() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("bad.toml");
    std::fs::write(&cfg, "[server]\nhttp_bind = \"127.0.0.1:0\"\nhttp_bnid = 1\n").unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_caudal")).args(["check"]).arg(&cfg).output().unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("http_bnid") && err.contains("line 3"), "{err}");
}
