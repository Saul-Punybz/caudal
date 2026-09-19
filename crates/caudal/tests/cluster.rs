//! Origin-edge clustering against the real binary: two origins take the
//! same stream over RTMP, an edge pulls it on its first viewer and serves it
//! as LL-HLS, then keeps its viewer going when the origin it pulls from is
//! killed.
//!
//! Runs only with `CAUDAL_E2E=1` and ffmpeg on PATH.

mod support;

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use support::{Publisher, Server, have};

const SECRET: &str = "e2e-cluster-shared-secret";

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

fn origin(id: &str) -> Server {
    Server::start_with(&format!("\n[cluster]\nrole = \"origin\"\nnode_id = \"{id}\"\nsecret = \"{SECRET}\"\n"))
}

fn edge(origins: &[&Server], secret: &str) -> Server {
    let list: Vec<String> = origins.iter().map(|o| format!("\"http://127.0.0.1:{}\"", o.http)).collect();
    Server::start_with(&format!(
        "\n[cluster]\nrole = \"edge\"\nnode_id = \"edge-1\"\nsecret = \"{secret}\"\norigins = [{}]\nidle_timeout_secs = 5\n",
        list.join(", ")
    ))
}

/// `ffmpeg -i <url> -t <secs> -f null -`, killed after `deadline`. Returns
/// (exited cleanly, frames decoded, stderr).
fn decode(url: &str, secs: u32, deadline: Duration) -> (bool, u64, String) {
    let mut child = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-loglevel", "info", "-i", url, "-t", &secs.to_string(), "-f", "null", "-"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ffmpeg");
    let mut stderr = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let t0 = Instant::now();
    let ok = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st.success();
        }
        if t0.elapsed() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break false;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let err = reader.join().unwrap();
    // The last progress line: "frame=  301 fps=…".
    let frames = err
        .rsplit("frame=")
        .next()
        .and_then(|t| t.trim_start().split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    (ok, frames, err)
}

/// A number after `"key":` in a JSON body (first occurrence).
fn json_u64(body: &str, key: &str) -> Option<u64> {
    body.split(&format!("\"{key}\":")).nth(1)?.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
}

fn metric_line<'a>(metrics: &'a str, prefix: &str) -> Option<&'a str> {
    metrics.lines().find(|l| l.starts_with(prefix))
}

#[test]
fn edge_pulls_on_first_viewer_and_survives_origin_loss() {
    if !enabled() {
        return;
    }
    let a = origin("origin-a");
    let b = origin("origin-b");
    let pub_a = Publisher::rtmp(&a.rtmp_url("cam"), 90);
    let _pub_b = Publisher::rtmp(&b.rtmp_url("cam"), 90);
    a.wait_until("/api/v1/streams/cam", Duration::from_secs(20), |s| s.contains("\"h264\""));
    b.wait_until("/api/v1/streams/cam", Duration::from_secs(20), |s| s.contains("\"h264\""));

    let e = edge(&[&a, &b], SECRET);
    assert_eq!(e.get("/api/v1/streams/cam").unwrap().0, 404, "nothing pulled before a viewer");

    // First viewer: a player's first playlist request starts the pull.
    // ffmpeg's HLS demuxer ignores LL-HLS parts and gives up on a playlist
    // with no complete segment yet (true of any fresh stream, origin or
    // edge), so it joins once the first segment is listed.
    let t0 = Instant::now();
    let first = e.get("/hls/cam/index.m3u8").unwrap();
    assert_eq!(first.0, 200, "first playlist request on the edge: {first:?}");
    println!("edge: first playlist answered {:?} after the first request", t0.elapsed());
    e.wait_until("/hls/cam/index.m3u8", Duration::from_secs(20), |p| p.contains("#EXTINF"));
    println!("edge: first complete segment listed {:?} after the first request", t0.elapsed());
    let t0 = Instant::now();
    let (ok, frames, err) = decode(&e.url("/hls/cam/index.m3u8"), 10, Duration::from_secs(60));
    assert!(ok, "ffmpeg failed on the edge's LL-HLS:\n{err}");
    assert!(frames >= 250, "decoded {frames} frames in 10 s:\n{err}");
    println!("edge: 10 s decoded ({frames} frames) in {:?}", t0.elapsed());

    let streams = e.get("/api/v1/streams").unwrap().1;
    assert!(streams.contains("\"name\":\"cam\""), "{streams}");
    let metrics = e.get("/metrics").unwrap().1;
    let a_label = format!("origin=\"http://127.0.0.1:{}/\"", a.http);
    let pulls = metric_line(&metrics, "caudal_cluster_pulls{stream=\"cam\"").expect("pull metric");
    assert!(pulls.contains(&a_label), "pulling from the first origin: {pulls}");
    let setup = metric_line(&metrics, "caudal_cluster_pull_setup_seconds{stream=\"cam\"").expect("setup metric");
    println!("edge: {setup}");

    // Failover: a second viewer runs while origin A (the pull source) is
    // killed; origin B has the same stream.
    let viewer = {
        let url = e.url("/hls/cam/index.m3u8");
        std::thread::spawn(move || decode(&url, 20, Duration::from_secs(90)))
    };
    // frames_in on the edge, sampled every 50 ms, to measure the stall.
    let stop = Arc::new(AtomicBool::new(false));
    let sampler = {
        let stop = stop.clone();
        let url = e.url("/api/v1/streams/cam");
        std::thread::spawn(move || {
            let mut samples = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                if let Ok(mut r) = ureq::get(&url).call()
                    && let Ok(body) = r.body_mut().read_to_string()
                    && let Some(n) = json_u64(&body, "frames_in")
                {
                    samples.push((Instant::now(), n));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            samples
        })
    };
    std::thread::sleep(Duration::from_secs(5));
    let killed = Instant::now();
    drop(pub_a);
    drop(a);
    let (ok, frames, err) = viewer.join().unwrap();
    std::thread::sleep(Duration::from_secs(1));
    stop.store(true, Ordering::Relaxed);
    let samples = sampler.join().unwrap();

    // Longest time the edge's frame counter stood still after the kill.
    let mut stall = Duration::ZERO;
    let mut since: Option<(Instant, u64)> = None;
    for &(t, n) in samples.iter().filter(|(t, _)| *t >= killed) {
        match since {
            Some((t0, n0)) if n == n0 => stall = stall.max(t - t0),
            _ => since = Some((t, n)),
        }
    }
    println!("failover: edge ingest stalled {stall:?} after origin A was killed");
    println!("failover: viewer decoded {frames} frames of 20 s, exited cleanly: {ok}");

    let metrics = e.get("/metrics").unwrap().1;
    let b_label = format!("origin=\"http://127.0.0.1:{}/\"", b.http);
    let pulls = metric_line(&metrics, "caudal_cluster_pulls{stream=\"cam\"").expect("pull metric after failover");
    assert!(pulls.contains(&b_label), "pulling from origin B now: {pulls}");
    assert!(metrics.contains("caudal_cluster_failovers_total{stream=\"cam\"} 1"), "{metrics}");
    if let Some(l) = metric_line(&metrics, "caudal_cluster_pull_setup_seconds{stream=\"cam\"") {
        println!("failover: {l}");
    }
    assert!(stall < Duration::from_secs(5), "edge stalled {stall:?}");
    assert!(ok, "the viewer did not survive the failover:\n{err}");
    assert!(frames >= 450, "viewer decoded only {frames} frames of 20 s:\n{err}");
}

#[test]
fn edge_with_a_wrong_secret_gets_nothing() {
    if !enabled() {
        return;
    }
    let a = origin("origin-a");
    let _p = Publisher::rtmp(&a.rtmp_url("cam"), 40);
    a.wait_until("/api/v1/streams/cam", Duration::from_secs(20), |s| s.contains("\"h264\""));
    let e = edge(&[&a], "not-the-cluster-secret");
    assert_eq!(e.get("/hls/cam/index.m3u8").unwrap().0, 404);
    assert_eq!(e.get("/api/v1/streams/cam").unwrap().0, 404);
    // And the origin refuses the locate call itself without the secret.
    assert_eq!(a.get("/api/v1/cluster/locate/cam").unwrap().0, 401);
}
