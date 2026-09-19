//! End-to-end: `caudal import-mist` on a MistServer config with a
//! `push://` RTMP stream, then the *real* `caudal` binary started with the
//! generated `caudal.toml`, published to with ffmpeg, and checked over
//! `/api/v1/streams` — the same "does the imported config actually work"
//! path a MistServer operator would take.
//!
//! Runs only with `CAUDAL_E2E=1` and ffmpeg on PATH (see `tests/e2e.rs`).
//! Every port is picked free at run time: nothing here binds a MistServer
//! or Caudal default port, so it is safe next to other agents' runs.

mod support;

use std::process::{Command, Stdio};
use std::time::Duration;

use support::{Publisher, free_port, free_udp_port, have};

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
fn imported_mist_config_accepts_a_real_rtmp_publish() {
    if !enabled() {
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let http = free_port();
    let rtmp = free_port();

    // A MistServer config with an RTMP connector and an open `push://`
    // stream — the smallest realistic input this importer maps to a
    // working live path (see tests/fixtures/mist/README.md for the schema
    // sources). Ports are free ones chosen just above, not MistServer's
    // usual 1935/8080, so this never collides with anything else running
    // on the machine.
    let mist_json = dir.path().join("config.json");
    std::fs::write(
        &mist_json,
        format!(
            r#"{{"config":{{"protocols":[
                {{"connector":"RTMP","port":{rtmp}}},
                {{"connector":"HTTP","port":{http}}}
            ]}},"streams":{{"e2emist":{{"source":"push://"}}}}}}"#
        ),
    )
    .unwrap();

    let caudal_toml = dir.path().join("caudal.toml");
    let import = Command::new(env!("CARGO_BIN_EXE_caudal"))
        .arg("import-mist")
        .arg(&mist_json)
        .arg("-o")
        .arg(&caudal_toml)
        .output()
        .expect("run caudal import-mist");
    assert!(import.status.success(), "import-mist failed: {}", String::from_utf8_lossy(&import.stderr));

    // The import itself only configures RTMP + HTTP (all the fixture
    // asked for); every other listener still defaults on, so give SRT,
    // WebRTC and MoQ free ports too instead of their MistServer-adjacent
    // defaults (9000/8189/4443) — the same reason tests/support/mod.rs's
    // `Server::start_with` always overrides them.
    let (srt, webrtc, moq) = (free_udp_port(), free_udp_port(), free_udp_port());
    let mut cfg = std::fs::read_to_string(&caudal_toml).unwrap();
    cfg.push_str(&format!(
        "\n[srt]\nbind = \"127.0.0.1:{srt}\"\n\n[webrtc]\nudp_bind = \"127.0.0.1:{webrtc}\"\n\n[moq]\nbind = \"127.0.0.1:{moq}\"\n"
    ));
    std::fs::write(&caudal_toml, cfg).unwrap();

    // `import-mist` already validated its own output; this is the second,
    // independent check the plan asks for: the real `caudal check`.
    let check = Command::new(env!("CARGO_BIN_EXE_caudal")).arg("check").arg(&caudal_toml).output().unwrap();
    assert!(
        check.status.success(),
        "caudal check on the imported+extended config failed: {}",
        String::from_utf8_lossy(&check.stderr)
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_caudal"))
        .arg("--config")
        .arg(&caudal_toml)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn caudal");

    let url = format!("http://127.0.0.1:{http}");
    let healthy = {
        let t0 = std::time::Instant::now();
        loop {
            if let Ok(r) = ureq::get(format!("{url}/healthz")).call()
                && r.status() == 200
            {
                break true;
            }
            if t0.elapsed() > Duration::from_secs(10) {
                break false;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    assert!(healthy, "caudal (imported config) never answered /healthz");

    // `[rtmp] app = "live"` is Caudal's default and what `import-mist`
    // writes (MistServer's RTMP connector has no equivalent app-name
    // setting — see the import report); the stream name is the Mist
    // stream this config's `push://` source was for.
    let publish_url = format!("rtmp://127.0.0.1:{rtmp}/live/e2emist");
    let _publisher = Publisher::rtmp(&publish_url, 20);

    let t0 = std::time::Instant::now();
    let body = loop {
        if let Ok(r) = ureq::get(format!("{url}/api/v1/streams")).call() {
            let mut r = r;
            let body = r.body_mut().read_to_string().unwrap_or_default();
            if body.contains("\"e2emist\"") {
                break body;
            }
        }
        assert!(t0.elapsed() < Duration::from_secs(20), "e2emist never appeared in /api/v1/streams");
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(body.contains("e2emist"), "{body}");

    let _ = child.kill();
    let _ = child.wait();
}
