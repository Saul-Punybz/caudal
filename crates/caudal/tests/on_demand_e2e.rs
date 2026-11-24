//! On-demand pulls end to end, against the real binary: an LL-HLS player
//! request through the router starts an on-demand `[[http.pull]]` (ffmpeg
//! serving MPEG-TS with `-listen 1`), `GET /api/v1/pulls` shows it, and it
//! stops after `idle_secs` once the player is gone. An unknown name gets a
//! 404 and starts nothing.
//!
//! Runs only with `CAUDAL_E2E=1` and ffmpeg on PATH, like `e2e.rs`.

#![cfg(unix)]

mod support;

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use support::{Server, free_port, have};

struct Source(Child);

impl Drop for Source {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn pull_state(s: &Server, stream: &str) -> Option<String> {
    let (_, body) = s.get("/api/v1/pulls").ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    v.as_array()?.iter().find(|p| p["stream"] == stream).map(|p| p["state"].as_str().unwrap_or("").to_owned())
}

#[test]
fn a_player_starts_an_on_demand_pull_and_leaving_stops_it() {
    if std::env::var("CAUDAL_E2E").is_err() {
        eprintln!("SKIP: set CAUDAL_E2E=1 to run end-to-end tests");
        return;
    }
    if !have("ffmpeg") {
        eprintln!("SKIP: ffmpeg not on PATH");
        return;
    }
    let url = format!("http://127.0.0.1:{}/stream.ts", free_port());
    let s = Server::start_with(&format!(
        "\n[[http.pull]]\nstream = \"web\"\nurl = \"{url}\"\non_demand = true\nidle_secs = 2\n"
    ));
    let _source = Source(
        Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-re", "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=25"])
            .args(["-c:v", "libx264", "-preset", "veryfast", "-g", "25", "-t", "120", "-f", "mpegts", "-listen", "1"])
            .arg(&url)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ffmpeg"),
    );
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(pull_state(&s, "web").as_deref(), Some("idle"));
    assert_eq!(s.get("/api/v1/streams/web").unwrap().0, 404, "not pulled before a viewer asks");

    // Unknown: 404, nothing started.
    assert_eq!(s.get("/hls/nope/index.m3u8").unwrap().0, 404);

    // The player's first playlist request starts the pull.
    let playlist = s.wait_until("/hls/web/index.m3u8", Duration::from_secs(30), |b| b.contains("#EXT-X-PART"));
    assert!(playlist.contains("#EXTM3U"));
    assert_eq!(pull_state(&s, "web").as_deref(), Some("live"));
    let (_, metrics) = s.get("/metrics").unwrap();
    assert!(metrics.contains("caudal_on_demand_starts_total 1"), "{metrics}");

    // The player leaves: HLS counts it for 10 s more, then idle_secs.
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(40) {
        if s.get("/api/v1/streams/web").unwrap().0 == 404 && pull_state(&s, "web").as_deref() == Some("idle") {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    assert_eq!(s.get("/api/v1/streams/web").unwrap().0, 404, "stopped after the last viewer left");
    assert_eq!(pull_state(&s, "web").as_deref(), Some("idle"));
    let (_, metrics) = s.get("/metrics").unwrap();
    assert!(metrics.contains("caudal_on_demand_idle_stops_total 1"), "{metrics}");
}
