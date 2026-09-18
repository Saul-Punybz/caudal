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
        b.split("\"frames_in\":")
            .nth(1)
            .and_then(|t| t.split(|c: char| !c.is_ascii_digit()).next())
            .and_then(|n| n.parse().ok())
            .unwrap_or(0)
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
    for tag in [
        "#EXTM3U",
        "#EXT-X-VERSION",
        "#EXT-X-SERVER-CONTROL",
        "#EXT-X-PART-INF",
        "#EXT-X-PART",
        "#EXT-X-MAP",
        "#EXT-X-PROGRAM-DATE-TIME",
    ] {
        assert!(playlist.contains(tag), "playlist lacks {tag}:\n{playlist}");
    }
    assert!(playlist.contains("CAN-BLOCK-RELOAD=YES"), "blocking reload is on by default");

    // Init segment and the newest part must be fetchable fMP4.
    let init = s.get_bytes("/hls/e2e/init.mp4");
    assert_eq!(&init[4..8], b"ftyp", "init.mp4 starts with ftyp");
    let part = playlist
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix("#EXT-X-PART:"))
        .and_then(|l| l.split("URI=\"").nth(1))
        .and_then(|u| u.split('"').next())
        .expect("a part URI");
    let bytes = s.get_bytes(&format!("/hls/e2e/{part}"));
    assert!(bytes.windows(4).any(|w| w == b"moof"), "part is a CMAF fragment");

    // Blocking reload (RFC 8216bis 6.2.5.2). Re-read the playlist right now,
    // then ask for part 0 of the segment AFTER the one in progress: it is up
    // to a full segment away, so it cannot exist yet even on a slow CI runner.
    // (Asking for the next part of the current segment raced on GitHub
    // runners; asking for part 0 of the current one raced locally.)
    let edge = |pl: &str| -> u64 {
        let ms: u64 =
            pl.lines().find_map(|l| l.strip_prefix("#EXT-X-MEDIA-SEQUENCE:")).unwrap().trim().parse().unwrap();
        ms + pl.lines().filter(|l| l.starts_with("#EXTINF")).count() as u64
    };
    let fresh = s.get("/hls/e2e/index.m3u8").unwrap().1;
    let want = edge(&fresh) + 1;
    let t0 = Instant::now();
    let (code, answered) = s.get(&format!("/hls/e2e/index.m3u8?_HLS_msn={want}&_HLS_part=0")).unwrap();
    assert_eq!(code, 200);
    assert!(
        t0.elapsed() >= Duration::from_millis(200),
        "a part one segment ahead must block, answered in {:?}",
        t0.elapsed()
    );
    assert!(edge(&answered) >= want, "the blocked answer must include segment {want} in progress:\n{answered}");

    // A viewer joining now must have a join point within 2 s of playlist age.
    let joined = s.wait_until("/hls/e2e/index.m3u8", Duration::from_secs(5), |b| b.contains("#EXT-X-PART"));
    assert!(joined.contains("INDEPENDENT=YES"), "at least one independent part to join on");

    // Apple's validator, when installed (macOS with Xcode tools).
    let dir = tempfile::tempdir().unwrap();
    // Players and the validator enter through the multivariant playlist.
    let (code, master) = s.get("/hls/e2e/master.m3u8").unwrap();
    assert_eq!(code, 200);
    assert!(
        master.contains("CODECS=\"avc1.") && master.contains("mp4a.40.2") && master.contains("index.m3u8"),
        "{master}"
    );
    match support::validate_hls(&s.url("/hls/e2e/master.m3u8"), dir.path(), "ll_hls") {
        Some(Ok(())) => {}
        Some(Err(report)) => panic!("mediastreamvalidator reported errors:\n{report}"),
        None => {
            eprintln!("NOT VERIFIED: mediastreamvalidator not installed; LL-HLS conformance unchecked on this machine")
        }
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

/// Canary: the validator must FAIL on a broken playlist. Proves the tool is
/// installed and actually checking, not just exiting 0.
#[test]
fn validator_rejects_a_broken_playlist() {
    if std::env::var("CAUDAL_E2E").is_err() {
        return;
    }
    if !support::have_validator() {
        eprintln!("NOT VERIFIED: mediastreamvalidator not installed; canary skipped");
        return;
    }
    // Segment URI that 404s, target duration shorter than the segment, no
    // ENDLIST on a VOD-looking list: several independent errors.
    let port = support::serve_static(
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n#EXTINF:6.0,\nmissing.ts\n",
        "application/vnd.apple.mpegurl",
    );
    let dir = tempfile::tempdir().unwrap();
    match support::validate_hls(&format!("http://127.0.0.1:{port}/bad.m3u8"), dir.path(), "canary") {
        Some(Err(_)) => {}
        Some(Ok(())) => panic!("mediastreamvalidator accepted a broken playlist; it is not really validating"),
        None => unreachable!("have_validator() was true"),
    }
}

/// The parser must read Apple's real output format. Fixture is a trimmed
/// copy of an actual mediastreamvalidator 1.26 log.
#[test]
fn validator_output_parser() {
    let log = "\
----------------------------------------------------------------------------------------------------
                                          CRITICAL Errors
----------------------------------------------------------------------------------------------------
-12642: Max EXTINF duration mroe than twice target duration
----------------------------------------------------------------------------------------------------
                                      MUST Fix HLS Spec Issues
----------------------------------------------------------------------------------------------------
-50120: Content not delivered via HTTP/2
-50125: Low-latency playlist MUST declare EXT-X-RENDITION-REPORT tags
----------------------------------------------------------------------------------------------------
                                     SHOULD Fix HLS Spec Issues
----------------------------------------------------------------------------------------------------
-50102: PART-HOLD-BACK SHOULD be at least three times the Part Target Duration
";
    let got = support::validator_blocking_issues(log);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].starts_with("-12642"), "critical errors block; allow-listed MUSTs and SHOULDs do not");
}

#[test]
fn srt_publish_plays_as_ll_hls() {
    if !enabled() {
        return;
    }
    if !have("srt-live-transmit") {
        eprintln!("SKIP: srt-live-transmit not on PATH (brew install srt)");
        return;
    }
    let s = Server::start();
    let _publ = Publisher::srt(&s.srt_url("srt1"), 40);

    let body = s.wait_until("/api/v1/streams/srt1", Duration::from_secs(20), |b| {
        b.contains("\"h264\"") && b.contains("\"aac\"")
    });
    assert!(body.contains("\"width\":1280") && body.contains("\"height\":720"), "{body}");

    let playlist = s.wait_until("/hls/srt1/index.m3u8", Duration::from_secs(20), |b| b.contains("#EXT-X-PART"));
    assert!(playlist.contains("INDEPENDENT=YES"), "{playlist}");
    let init = s.get_bytes("/hls/srt1/init.mp4");
    assert_eq!(&init[4..8], b"ftyp");
    let (code, master) = s.get("/hls/srt1/master.m3u8").unwrap();
    assert_eq!(code, 200);
    assert!(master.contains("avc1.") && master.contains("mp4a.40.2"), "{master}");
}

#[test]
fn ui_is_served_with_spa_fallback() {
    if !enabled() {
        return;
    }
    let s = Server::start();
    let (code, root) = s.get("/").unwrap();
    assert_eq!(code, 200);
    assert!(root.contains("<html") || root.contains("<!doctype"), "{root}");
    // Client-side routes load the app too; missing files stay 404.
    assert_eq!(s.get("/streams/anything").unwrap().0, 200);
    assert_eq!(s.get("/assets/missing-file.js").unwrap().0, 404);
    // API and HLS routes still win over the fallback.
    assert_eq!(s.get("/api/v1/streams").unwrap().1.trim(), "[]");
}

/// Viewers are people watching, not internal readers. Caught 18 Sep 2026:
/// the LL-HLS packager's own subscription showed up as "1 viewer".
#[test]
fn hls_viewers_count_players_not_the_packager() {
    if !enabled() {
        return;
    }
    let s = Server::start();
    let _publ = Publisher::rtmp(&s.rtmp_url("v"), 60);
    // Wait on the API, not the playlist: polling the playlist is exactly what
    // a player does, so it would count this test as a viewer.
    s.wait_until("/api/v1/streams/v", Duration::from_secs(20), |b| b.contains("\"h264\""));
    let viewers = |s: &Server| -> u64 {
        let b = s.get("/api/v1/streams/v").unwrap().1;
        b.split("\"viewers\":")
            .nth(1)
            .and_then(|t| t.split(|c: char| !c.is_ascii_digit()).next())
            .and_then(|n| n.parse().ok())
            .unwrap()
    };
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(viewers(&s), 0, "nobody is watching yet");

    // One player polling the playlist like hls.js does.
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(3) {
        let _ = s.get("/hls/v/index.m3u8");
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(viewers(&s), 1, "one player polling");

    // It leaves; after the idle window the count drops back.
    std::thread::sleep(Duration::from_secs(12));
    assert_eq!(viewers(&s), 0, "the player left");
}
