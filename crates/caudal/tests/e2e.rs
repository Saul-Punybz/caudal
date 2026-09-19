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

    // An SCTE-35 cue inserted over the API shows up as EXT-X-DATERANGE,
    // and is part of what the validator checks below.
    let mut resp = ureq::post(s.url("/api/v1/streams/e2e/cues"))
        .header("content-type", "application/json")
        .send(r#"{"kind":"out","duration_ms":30000}"#)
        .expect("POST cue");
    assert_eq!(resp.status().as_u16(), 202);
    let cue: serde_json::Value = serde_json::from_str(&resp.body_mut().read_to_string().unwrap()).unwrap();
    let hex = cue["section_hex"].as_str().expect("section_hex").to_owned();
    let with_cue = s.wait_until("/hls/e2e/index.m3u8", Duration::from_secs(5), |b| b.contains("#EXT-X-DATERANGE"));
    let dr = with_cue.lines().find(|l| l.starts_with("#EXT-X-DATERANGE:")).unwrap();
    assert!(dr.contains(&format!("SCTE35-OUT={hex}")) && dr.contains("PLANNED-DURATION=30.000"), "{dr}");

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

const SECRET: &str = "0123456789abcdef0123456789abcdef-e2e";

/// A signed HS256 token for `sub` allowing `act`, valid for an hour.
fn token(sub: &str, act: &[&str]) -> String {
    let exp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + 3600;
    let claims = serde_json::json!({ "sub": sub, "act": act, "exp": exp });
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(SECRET.as_bytes()),
    )
    .unwrap()
}

#[test]
fn publishing_needs_a_valid_token_when_auth_is_on() {
    if !enabled() {
        return;
    }
    let s = Server::start_with(&format!("\n[auth]\nsecret = \"{SECRET}\"\npublish = true\n"));

    // No token: refused, nothing appears.
    let _anon = Publisher::rtmp(&s.rtmp_url("guarded"), 10);
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(s.get("/api/v1/streams/guarded").unwrap().0, 404, "no token, no stream");

    // A token for another stream: refused.
    let _other = Publisher::rtmp(&format!("{}?token={}", s.rtmp_url("guarded"), token("elsewhere", &["publish"])), 10);
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(s.get("/api/v1/streams/guarded").unwrap().0, 404, "token for another stream");

    // The right token: publishes.
    let _ok = Publisher::rtmp(&format!("{}?token={}", s.rtmp_url("guarded"), token("guarded", &["publish"])), 20);
    s.wait_until("/api/v1/streams/guarded", Duration::from_secs(15), |b| b.contains("\"h264\""));
}

#[test]
fn playing_needs_a_token_and_every_playlist_uri_carries_it() {
    if !enabled() {
        return;
    }
    let s = Server::start_with(&format!("\n[auth]\nsecret = \"{SECRET}\"\npublish = false\nplay = true\n"));
    let _publ = Publisher::rtmp(&s.rtmp_url("paid"), 30);
    s.wait_until("/api/v1/streams/paid", Duration::from_secs(15), |b| b.contains("\"h264\""));

    assert_eq!(s.get("/hls/paid/master.m3u8").unwrap().0, 401, "no token");
    assert_eq!(s.get("/hls/paid/master.m3u8?token=not.a.jwt").unwrap().0, 403, "garbage token");
    assert_eq!(
        s.get(&format!("/hls/paid/master.m3u8?token={}", token("paid", &["publish"]))).unwrap().0,
        403,
        "publish-only token"
    );

    let t = token("paid", &["play"]);
    let playlist = s
        .wait_until(&format!("/hls/paid/index.m3u8?token={t}"), Duration::from_secs(15), |b| b.contains("#EXT-X-PART"));
    let with_t = format!("token={t}");
    assert!(playlist.contains(&format!("init.mp4?{with_t}")), "init URI carries the token:\n{playlist}");
    let part = playlist
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix("#EXT-X-PART:"))
        .and_then(|l| l.split("URI=\"").nth(1))
        .and_then(|u| u.split('"').next())
        .expect("a part URI")
        .to_owned();
    assert!(part.contains(&with_t), "{part}");
    assert_eq!(s.get(&format!("/hls/paid/{part}")).unwrap().0, 200, "part with its token");
    let bare = part.split('?').next().unwrap();
    assert_eq!(s.get(&format!("/hls/paid/{bare}")).unwrap().0, 401, "part without a token");
}

#[test]
fn https_speaks_http2() {
    if !enabled() {
        return;
    }
    if !have("curl") {
        eprintln!("SKIP: curl not on PATH");
        return;
    }
    let certs = tempfile::tempdir().unwrap();
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()]).unwrap();
    let (cert, key) = (certs.path().join("cert.pem"), certs.path().join("key.pem"));
    std::fs::write(&cert, ck.cert.pem()).unwrap();
    std::fs::write(&key, ck.signing_key.serialize_pem()).unwrap();
    let https = support::free_port();
    let s = Server::start_with(&format!(
        "\n[tls]\nbind = \"127.0.0.1:{https}\"\ncert = \"{}\"\nkey = \"{}\"\n",
        cert.display(),
        key.display()
    ));
    let _ = &s;
    let url = format!("https://127.0.0.1:{https}/healthz");
    let t0 = Instant::now();
    let version = loop {
        let out = std::process::Command::new("curl")
            .args(["-sk", "--http2", "-o", "/dev/null", "-w", "%{http_version}", &url])
            .output()
            .unwrap();
        let v = String::from_utf8_lossy(&out.stdout).to_string();
        if v == "2" || t0.elapsed() > Duration::from_secs(10) {
            break v;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(version, "2", "HTTPS must negotiate HTTP/2 via ALPN");
}

/// WHIP in (ffmpeg's WHIP muxer, H.264 + Opus), LL-HLS out. Red until batch
/// 4 lands WHIP ingest (caudal-webrtc) and Opus in fMP4 (caudal-hls).
#[test]
fn whip_publish_plays_as_ll_hls() {
    if !enabled() {
        return;
    }
    let muxers = std::process::Command::new("ffmpeg").args(["-hide_banner", "-muxers"]).output().unwrap();
    if !String::from_utf8_lossy(&muxers.stdout).contains("whip") {
        eprintln!("SKIP: this ffmpeg has no WHIP muxer (needs ffmpeg >= 8)");
        return;
    }
    let s = Server::start();
    let _publ = Publisher::whip(&s.whip_url("w1"), 40);
    let body = s.wait_until("/api/v1/streams/w1", Duration::from_secs(20), |b| {
        b.contains("\"h264\"") && b.contains("\"opus\"")
    });
    assert!(body.contains("\"width\":1280"), "{body}");
    let playlist = s.wait_until("/hls/w1/index.m3u8", Duration::from_secs(20), |b| b.contains("#EXT-X-PART"));
    assert!(playlist.contains("INDEPENDENT=YES"), "{playlist}");
    let (code, master) = s.get("/hls/w1/master.m3u8").unwrap();
    assert_eq!(code, 200);
    assert!(master.contains("avc1.") && master.contains("opus"), "{master}");
}

/// Record a publish, replay it as VOD, cut a clip. Red until batch 6 R lands.
#[test]
fn recording_becomes_vod_and_clips_download() {
    if !enabled() {
        return;
    }
    let s = Server::start_with("\n[record]\nenabled = true\ndir = \"{dir}/rec\"\nsegment_secs = 2\n");
    {
        let _publ = Publisher::rtmp(&s.rtmp_url("recme"), 10);
        s.wait_until("/api/v1/streams/recme", Duration::from_secs(15), |b| b.contains("\"h264\""));
        std::thread::sleep(Duration::from_secs(9));
    }
    // The publish ended; the recording closes with ENDLIST.
    let list = s.wait_until("/api/v1/recordings", Duration::from_secs(15), |b| {
        b.contains("\"recme\"") && b.contains("\"ended_at\":\"")
    });
    let id = list.split("\"id\":\"").nth(1).and_then(|t| t.split('"').next()).expect("a recording id").to_owned();
    let vod =
        s.wait_until(&format!("/vod/recme/{id}/index.m3u8"), Duration::from_secs(10), |b| b.contains("#EXT-X-ENDLIST"));
    assert!(vod.contains("#EXT-X-PLAYLIST-TYPE:VOD") && vod.contains("#EXT-X-MAP"), "{vod}");
    assert!(vod.matches("#EXTINF").count() >= 3, "8+ s at 2 s segments:\n{vod}");

    // Clip [2 s, 5 s) as a progressive MP4.
    let resp = ureq::post(s.url("/api/v1/clips"))
        .header("Content-Type", "application/json")
        .send(format!("{{\"stream\":\"recme\",\"id\":\"{id}\",\"from_ms\":2000,\"to_ms\":5000}}"))
        .expect("clip request");
    assert_eq!(resp.status().as_u16(), 200);
    let mp4 = resp.into_body().read_to_vec().unwrap();
    assert_eq!(&mp4[4..8], b"ftyp");
    assert!(
        mp4.windows(4).any(|w| w == b"moov") && mp4.windows(4).any(|w| w == b"stco" || w == b"co64"),
        "progressive MP4 with sample tables"
    );

    // No path traversal through the VOD route.
    assert!(matches!(s.get("/vod/recme/..%2F..%2Fetc/passwd").unwrap().0, 400 | 404));
}

#[test]
fn access_rule_denies_rtmp_publish_from_a_matching_cidr() {
    if !enabled() {
        return;
    }
    // Every e2e client is loopback, so a rule naming 127.0.0.1/32 denies it
    // and one naming an unrelated network (10.0.0.0/8) never does.
    let s = Server::start_with("\n[[access.rules]]\nstreams = [\"*\"]\npublish_deny = [\"127.0.0.1/32\"]\n");
    let _publ = Publisher::rtmp(&s.rtmp_url("blocked"), 5);
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(s.get("/api/v1/streams/blocked").unwrap().0, 404, "publish from a denied IP never creates a stream");

    let (status, body) = s.get("/metrics").unwrap();
    assert_eq!(status, 200);
    assert!(
        body.contains("caudal_access_denied_total{stream=\"blocked\",reason=\"ip_denied\"} 1"),
        "denial must be counted and visible at /metrics:\n{body}"
    );
}

#[test]
fn access_rule_with_a_non_matching_cidr_never_blocks_the_real_client() {
    if !enabled() {
        return;
    }
    let s = Server::start_with(
        "\n[[access.rules]]\nstreams = [\"*\"]\npublish_deny = [\"10.0.0.0/8\"]\nplay_deny = [\"10.0.0.0/8\"]\n",
    );
    let _publ = Publisher::rtmp(&s.rtmp_url("ok"), 20);
    s.wait_until("/api/v1/streams/ok", Duration::from_secs(15), |b| b.contains("\"h264\""));
    // `wait_until` itself only ever succeeds on a 200, so reaching this
    // point already proves loopback plays: only 10.0.0.0/8 is denied.
    s.wait_until("/hls/ok/index.m3u8", Duration::from_secs(15), |_| true);
}

#[test]
fn access_rule_denies_hls_play_from_a_matching_cidr() {
    if !enabled() {
        return;
    }
    // Publish is unguarded here; only play is denied, so the stream exists
    // and only the read side is refused.
    let s = Server::start_with("\n[[access.rules]]\nstreams = [\"*\"]\nplay_deny = [\"127.0.0.1/32\"]\n");
    let _publ = Publisher::rtmp(&s.rtmp_url("watched"), 15);
    s.wait_until("/api/v1/streams/watched", Duration::from_secs(15), |b| b.contains("\"h264\""));

    assert_eq!(s.get("/hls/watched/index.m3u8").unwrap().0, 403, "play from a denied IP is refused");
    let (status, body) = s.get("/metrics").unwrap();
    assert_eq!(status, 200);
    assert!(body.contains("caudal_access_denied_total{stream=\"watched\",reason=\"ip_denied\"}"), "{body}");
}

/// Audit gap 4: an encoder drops and republishes the same name within the
/// reconnect grace (default 10 s). The LL-HLS playlist must stay live
/// across the gap (no ENDLIST, media sequence never goes back), mark the
/// seam with `EXT-X-DISCONTINUITY`, and a real player (ffmpeg's HLS demuxer)
/// must decode straight through it without exiting.
#[test]
fn ll_hls_survives_a_publisher_reconnect() {
    if !enabled() {
        return;
    }
    let s = Server::start();
    let first = Publisher::rtmp(&s.rtmp_url("re"), 60);
    s.wait_until("/hls/re/index.m3u8", Duration::from_secs(20), |b| b.matches("#EXTINF").count() >= 3);

    // A viewer that must decode 20 s of video. It joins 3 segments from the
    // live edge, so it cannot get there on the first publish alone (about
    // 10 s of which remain for it): it has to cross the reconnect.
    const FRAMES: u32 = 600;
    let reader = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-nostats", "-loglevel", "error", "-i"])
        .arg(s.url("/hls/re/index.m3u8"))
        .args(["-map", "0:v:0", "-frames:v", &FRAMES.to_string(), "-progress", "pipe:1", "-f", "null", "-"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn ffmpeg reader");
    let reader = KillOnDrop(Some(reader));

    // Watch the playlist from before the drop until the new publish is on
    // it: never ended, never renumbered.
    let mut last_seq = 0u64;
    let mut watch = |until: Duration| -> Option<String> {
        let t0 = Instant::now();
        let mut last = None;
        while t0.elapsed() < until {
            let (code, pl) = s.get("/hls/re/index.m3u8").unwrap();
            assert_eq!(code, 200, "playlist must keep answering across the reconnect");
            assert!(!pl.contains("#EXT-X-ENDLIST"), "ended inside the grace:\n{pl}");
            let seq: u64 = pl
                .lines()
                .find_map(|l| l.strip_prefix("#EXT-X-MEDIA-SEQUENCE:"))
                .and_then(|v| v.trim().parse().ok())
                .expect("media sequence");
            assert!(seq >= last_seq, "media sequence went back {last_seq} -> {seq}:\n{pl}");
            last_seq = seq;
            if pl.contains("#EXT-X-DISCONTINUITY\n") {
                last = Some(pl);
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        last
    };
    watch(Duration::from_secs(3));
    drop(first);
    // The encoder takes a few seconds to come back.
    watch(Duration::from_secs(3));
    let second = Publisher::rtmp(&s.rtmp_url("re"), 60);
    let seam = watch(Duration::from_secs(20)).expect("the republish shows up after an EXT-X-DISCONTINUITY");
    let after = seam.split("#EXT-X-DISCONTINUITY\n").nth(1).unwrap();
    let before = seam.split("#EXT-X-DISCONTINUITY\n").next().unwrap();
    let last_old = before.lines().rev().find(|l| l.ends_with(".m4s") && !l.starts_with('#')).expect("old segment");
    let first_new = after
        .lines()
        .find_map(|l| l.split("URI=\"").nth(1).and_then(|u| u.split('"').next()).or(l.ends_with(".m4s").then_some(l)))
        .expect("new segment");
    let msn = |u: &str| -> u64 { u[1..].split('.').next().unwrap().parse().unwrap() };
    assert_eq!(msn(first_new), msn(last_old) + 1, "numbers keep counting across the seam:\n{seam}");

    // Apple's validator on the playlist with the seam in it.
    let dir = tempfile::tempdir().unwrap();
    match support::validate_hls(&s.url("/hls/re/master.m3u8"), dir.path(), "ll_hls_reconnect") {
        Some(Ok(())) => {}
        Some(Err(report)) => panic!("mediastreamvalidator reported errors across a reconnect:\n{report}"),
        None => eprintln!("NOT VERIFIED: mediastreamvalidator not installed; reconnect conformance unchecked"),
    }

    // The viewer decodes all its frames and exits cleanly.
    let out = reader.wait_timeout(Duration::from_secs(60)).expect("ffmpeg reader never finished");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let frames: u32 =
        stdout.lines().rev().find_map(|l| l.strip_prefix("frame=")).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
    assert!(out.status.success(), "ffmpeg reader failed: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(frames, FRAMES, "the reader stopped early (the playlist ended?)\n{stdout}");
    drop(second);
}

/// A child process killed when dropped, so a failed assertion never leaves
/// an ffmpeg behind.
struct KillOnDrop(Option<std::process::Child>);

impl KillOnDrop {
    /// Waits for exit up to `limit`; `None` (and the process killed) after.
    fn wait_timeout(mut self, limit: Duration) -> Option<std::process::Output> {
        let t0 = Instant::now();
        while t0.elapsed() < limit {
            if self.0.as_mut()?.try_wait().ok()?.is_some() {
                return self.0.take()?.wait_with_output().ok();
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        None
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}
