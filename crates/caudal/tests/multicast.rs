//! End-to-end: IP multicast output against the real binary. ffmpeg
//! publishes over RTMP; `[[multicast]]` sends the stream to two
//! 239.255.0.0/16 groups (MPEG-TS over UDP and RTP/MP2T) with
//! `IP_MULTICAST_LOOP` on; ffprobe and ffmpeg, as ordinary multicast
//! receivers on the same host, must find H.264 + AAC and decode it. Then a
//! hot reload removes the outputs. Runs only with `CAUDAL_E2E=1` and
//! ffmpeg/ffprobe on PATH, same gate as `e2e.rs`.
//!
//! Uses the default multicast interface (the default route): GitHub's Linux
//! runners deliver looped-back multicast on it, as does a Mac.

mod support;

use std::net::UdpSocket;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use support::{Publisher, Server, have};

fn enabled() -> bool {
    if std::env::var("CAUDAL_E2E").is_err() {
        eprintln!("SKIP: set CAUDAL_E2E=1 to run end-to-end tests");
        return false;
    }
    if !have("ffmpeg") || !have("ffprobe") {
        eprintln!("SKIP: ffmpeg/ffprobe not on PATH");
        return false;
    }
    true
}

/// A free UDP port and a 239.255.x.y group derived from it, so parallel
/// runs don't hear each other.
fn group() -> String {
    let port = UdpSocket::bind("0.0.0.0:0").unwrap().local_addr().unwrap().port();
    format!("239.255.{}.{}:{port}", 100 + port % 100, 1 + port % 250)
}

/// Runs `cmd`, killing it after `limit`. Returns (success, stdout, stderr).
fn run(cmd: &mut Command, limit: Duration) -> (bool, String, String) {
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("spawn");
    let t0 = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if t0.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let out = child.wait_with_output().unwrap();
    (
        status.is_some_and(|s| s.success()),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn probe(url: &str) -> String {
    let (ok, out, err) = run(
        Command::new("ffprobe").args(["-hide_banner", "-v", "error"]).args([
            "-show_entries",
            "stream=codec_name,width,height",
            "-of",
            "compact",
            url,
        ]),
        Duration::from_secs(20),
    );
    assert!(ok, "ffprobe {url} failed: {err}");
    out
}

/// Decodes `secs` from `url`; returns the number of video frames decoded.
fn decode(url: &str, secs: u32) -> u64 {
    let (ok, out, err) = run(
        Command::new("ffmpeg").args(["-hide_banner", "-v", "error", "-i", url, "-t", &secs.to_string()]).args([
            "-map",
            "0:v",
            "-f",
            "null",
            "-",
            "-progress",
            "pipe:1",
        ]),
        Duration::from_secs(u64::from(secs) + 20),
    );
    assert!(ok, "ffmpeg decode of {url} failed: {err}");
    // A receiver that joins mid-GOP sees non-IDR frames before its first
    // keyframe; anything else from the decoder is a real error.
    let unexpected: Vec<&str> = err
        .lines()
        .filter(|l| !l.contains("non-existing PPS") && !l.contains("no frame!") && !l.contains("Last message repeated"))
        .collect();
    assert!(unexpected.is_empty(), "decoder errors: {unexpected:?}");
    out.lines().filter_map(|l| l.strip_prefix("frame=")).filter_map(|n| n.trim().parse().ok()).max().unwrap_or(0)
}

#[test]
fn multicast_ts_and_rtp_decode_with_ffmpeg() {
    if !enabled() {
        return;
    }
    let (ts_group, rtp_group) = (group(), group());
    let s = Server::start_with(&format!(
        "\n[[multicast]]\nstream = \"mc\"\ngroup = \"{ts_group}\"\nttl = 1\nloopback = true\n\
         \n[[multicast]]\nstream = \"mc\"\ngroup = \"{rtp_group}\"\nformat = \"rtp\"\nttl = 1\nloopback = true\n"
    ));
    let (code, body) = s.get("/api/v1/multicast").unwrap();
    assert_eq!(code, 200);
    assert!(body.contains("\"state\":\"waiting\""), "outputs wait for the stream: {body}");

    let _publ = Publisher::rtmp(&s.rtmp_url("mc"), 60);
    let body = s.wait_until("/api/v1/multicast", Duration::from_secs(20), |b| {
        b.matches("\"state\":\"live\"").count() == 2 && !b.contains("\"packets_sent\":0,")
    });
    assert!(body.contains("\"send_errors\":0"), "{body}");

    // MPEG-TS over UDP: what a set-top box or VLC opens as udp://@group.
    let ts_url = format!("udp://{ts_group}?timeout=10000000");
    let streams = probe(&ts_url);
    assert!(streams.contains("codec_name=h264|width=1280|height=720"), "{streams}");
    assert!(streams.contains("codec_name=aac"), "{streams}");
    // 6 s at 30 fps, minus up to one 2 s GOP before the first keyframe.
    let frames = decode(&ts_url, 6);
    assert!(frames >= 90, "decoded {frames} frames from the TS group");

    // RTP/MP2T (payload type 33): ffmpeg's RTP demuxer needs no SDP for a
    // static payload type.
    let rtp_url = format!("rtp://{rtp_group}?timeout=10000000");
    let streams = probe(&rtp_url);
    assert!(streams.contains("codec_name=h264"), "{streams}");
    assert!(streams.contains("codec_name=aac"), "{streams}");
    let frames = decode(&rtp_url, 4);
    assert!(frames >= 45, "decoded {frames} frames from the RTP group");

    let (_, metrics) = s.get("/metrics").unwrap();
    assert!(metrics.contains("caudal_multicast_packets_total{stream=\"mc\""), "{metrics}");

    // Hot reload: dropping [[multicast]] stops both outputs, nothing else.
    let cfg = std::fs::read_to_string(&s.cfg_path).unwrap();
    let base = cfg.split("\n[[multicast]]").next().unwrap().to_owned();
    s.write_config(&base);
    let (code, body) = s.reload_via_api();
    assert_eq!(code, 200, "{body}");
    assert!(body.contains("\"applied\":[\"multicast\"]"), "{body}");
    let (_, body) = s.get("/api/v1/multicast").unwrap();
    assert_eq!(body.trim(), "[]");
}
