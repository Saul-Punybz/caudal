//! End-to-end: hot config reload against the real binary, both trigger
//! paths (`POST /api/v1/config/reload` and SIGHUP). Runs only with
//! `CAUDAL_E2E=1` and ffmpeg on PATH, same gate as `e2e.rs`.

mod support;

use std::time::Duration;

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

/// Reads `"frames_in":N` out of a `/api/v1/streams/{name}` body, the same
/// way `e2e.rs` proves a stream is still flowing.
fn frames_in(body: &str) -> u64 {
    body.split("\"frames_in\":")
        .nth(1)
        .and_then(|t| t.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

#[test]
fn reload_applies_a_changed_restream_target_without_dropping_the_stream() {
    if !enabled() {
        return;
    }
    let s = Server::start();
    let _publ = Publisher::rtmp(&s.rtmp_url("e2e"), 30);
    s.wait_until("/api/v1/streams/e2e", Duration::from_secs(15), |b| b.contains("\"h264\""));

    // No restream target configured yet.
    let (code, body) = s.get("/api/v1/restreams").unwrap();
    assert_eq!(code, 200);
    assert_eq!(body.trim(), "[]", "no restream targets configured yet");

    let before = frames_in(&s.get("/api/v1/streams/e2e").unwrap().1);

    // Add a `[[restream]]` target and reload over the api. The target URL
    // points nowhere real (port 1): that's fine, this only checks the
    // target is picked up and the source stream is undisturbed, not that
    // the push itself succeeds (caudal-restream's own tests cover that).
    let cfg = std::fs::read_to_string(&s.cfg_path).unwrap();
    s.write_config(&format!("{cfg}\n[[restream]]\nstream = \"e2e\"\nurl = \"rtmp://127.0.0.1:1/live/x\"\n"));
    let (code, body) = s.reload_via_api();
    assert_eq!(code, 200, "{body}");
    assert!(body.contains("\"applied\":[\"restream\"]"), "restream reported as applied live: {body}");
    assert!(body.contains("\"restarted\":[]"), "no listener should restart for a restream-only change: {body}");
    assert!(body.contains("\"requires_restart\":[]"), "{body}");

    // The new target shows up in the restream status list.
    let list = s.wait_until("/api/v1/restreams", Duration::from_secs(5), |b| b.contains("\"stream\":\"e2e\""));
    assert!(list.contains("\"target\""), "{list}");

    // The existing RTMP publisher was never interrupted: frames_in keeps
    // rising continuously. A dropped/reconnected session would either 404
    // the stream or reset frames_in near zero.
    std::thread::sleep(Duration::from_secs(2));
    let (code, after_body) = s.get("/api/v1/streams/e2e").unwrap();
    assert_eq!(code, 200, "the publisher must still be there after the reload");
    let after = frames_in(&after_body);
    assert!(after > before, "frames_in should keep rising across the reload, got {before} -> {after}");

    // The rtmp listener itself is untouched: still healthy.
    assert_eq!(s.get("/healthz").unwrap().0, 200);
}

#[test]
fn reload_via_sighup_also_applies() {
    if !enabled() {
        return;
    }
    let s = Server::start();
    let cfg = std::fs::read_to_string(&s.cfg_path).unwrap();
    s.write_config(&format!("{cfg}\n[[channel]]\nname = \"chan1\"\nitems = []\n"));
    s.sighup();
    s.wait_until("/api/v1/channels", Duration::from_secs(5), |b| b.contains("\"chan1\""));
}

#[test]
fn invalid_config_reload_is_rejected_and_everything_keeps_running() {
    if !enabled() {
        return;
    }
    let s = Server::start();
    let _publ = Publisher::rtmp(&s.rtmp_url("e2e"), 15);
    s.wait_until("/api/v1/streams/e2e", Duration::from_secs(15), |b| b.contains("\"h264\""));

    // Same unknown-key shape as `config_check_rejects_unknown_key_with_line_number`.
    s.write_config("[server]\nhttp_bnid = 1\n");
    let (code, body) = s.reload_via_api();
    assert_eq!(code, 400, "{body}");
    assert!(body.contains("http_bnid"), "{body}");

    // Nothing was touched: the running publisher and the server are both
    // still there.
    assert_eq!(s.get("/healthz").unwrap().0, 200);
    assert_eq!(s.get("/api/v1/streams/e2e").unwrap().0, 200, "stream must still be there after a rejected reload");
}

// The "no --config file to reload from" case (started with built-in
// defaults) is covered as a unit test in `src/reload.rs`, against a
// `Supervisor` built on port-0 binds — no real server, no fixed port to
// race with another test or another agent on this machine.
