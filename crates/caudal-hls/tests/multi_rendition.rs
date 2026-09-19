//! Real-socket check for adaptive-bitrate LL-HLS: two renditions of one
//! family (`abr` and `abr+low`) served over an actual TCP port and checked
//! with `ffprobe`, plus Apple's `mediastreamvalidator` when it is installed.
//! The playlist-shape assertions (master aggregation, rendition reports,
//! token propagation) already have thorough in-process coverage in
//! `src/tests.rs`; this file exists for the checks that need a real socket.

#[path = "../src/mp4demux.rs"]
mod mp4demux;

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use caudal_core::{BufferConfig, Registry};
use caudal_hls::{HlsConfig, router};

fn have(bin: &str) -> bool {
    Command::new("which").arg(bin).output().is_ok_and(|o| o.status.success())
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// Runs a short-lived tool in its own process group, force-killed on drop
/// (never waited-on-by-itself) — the same rule as every other spawned
/// process in this workspace, applied here even though `ffprobe` and
/// `mediastreamvalidator` are expected to exit on their own.
struct Guarded {
    child: Option<Child>,
}

impl Guarded {
    fn spawn(cmd: &mut Command) -> Self {
        use std::os::unix::process::CommandExt;
        let child =
            cmd.process_group(0).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("spawn guarded command");
        Self { child: Some(child) }
    }

    fn wait_with_output(mut self) -> std::process::Output {
        self.child.take().unwrap().wait_with_output().expect("wait for guarded command")
    }
}

impl Drop for Guarded {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let pgid = child.id();
            kill_group(pgid);
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

async fn wait_http(url: &str, pred: impl Fn(&str) -> bool, timeout: Duration) -> String {
    let t0 = tokio::time::Instant::now();
    loop {
        if let Ok(mut r) = ureq::get(url).call() {
            let body = r.body_mut().read_to_string().unwrap_or_default();
            if pred(&body) {
                return body;
            }
        }
        assert!(t0.elapsed() < timeout, "{url} never matched");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Two live streams (`abr`, `abr+low`) sharing one family, served over a
/// real TCP socket: the master lists both with relative URIs, each media
/// playlist reports on the other (never itself), tokens propagate
/// end-to-end, and `ffprobe -v error` finds nothing wrong with any of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abr_family_serves_and_ffprobes_clean() {
    let fx = mp4demux::demux(include_bytes!("fixtures/av.mp4"));
    let reg = Registry::new();
    let app = router(
        reg.clone(),
        HlsConfig { part_ms: 200, segment_ms: 2000, cue_tags: true, cue_out_tags: false },
        Vec::new(),
    );

    let port = free_port();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let root = reg.publish("abr", BufferConfig::default()).unwrap();
    root.set_tracks(fx.tracks.clone()).unwrap();
    let low = reg.publish("abr+low", BufferConfig::default()).unwrap();
    low.set_tracks(fx.tracks.clone()).unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    for n in 0..3 {
        for f in fx.looped(n) {
            root.push(f.clone()).unwrap();
            low.push(f).unwrap();
        }
    }

    let base = format!("http://127.0.0.1:{port}");
    wait_http(&format!("{base}/hls/abr/index.m3u8"), |b| b.contains("#EXTINF"), Duration::from_secs(10)).await;
    wait_http(&format!("{base}/hls/abr+low/index.m3u8"), |b| b.contains("#EXTINF"), Duration::from_secs(10)).await;

    // Master lists both, relative URIs, tokens carried through.
    let master = wait_http(
        &format!("{base}/hls/abr/master.m3u8?token=tkn.val.sig"),
        |b| b.contains("STREAM-INF"),
        Duration::from_secs(5),
    )
    .await;
    assert!(master.contains("../abr/index.m3u8?token=tkn.val.sig"), "{master}");
    assert!(master.contains("../abr+low/index.m3u8?token=tkn.val.sig"), "{master}");
    assert_eq!(master.matches("#EXT-X-STREAM-INF:").count(), 2, "{master}");

    // Each media playlist reports the other, never itself.
    let root_pl = ureq::get(format!("{base}/hls/abr/index.m3u8")).call().unwrap().body_mut().read_to_string().unwrap();
    assert!(root_pl.contains("#EXT-X-RENDITION-REPORT:URI=\"../abr+low/index.m3u8\""), "{root_pl}");
    assert!(!root_pl.contains("URI=\"../abr/index.m3u8\""), "must never report on itself:\n{root_pl}");
    let low_pl =
        ureq::get(format!("{base}/hls/abr+low/index.m3u8")).call().unwrap().body_mut().read_to_string().unwrap();
    assert!(low_pl.contains("#EXT-X-RENDITION-REPORT:URI=\"../abr/index.m3u8\""), "{low_pl}");
    assert!(!low_pl.contains("URI=\"../abr+low/index.m3u8\""), "must never report on itself:\n{low_pl}");

    if !have("ffprobe") {
        eprintln!("SKIP: ffprobe not on PATH");
    } else {
        let out =
            Guarded::spawn(Command::new("ffprobe").args(["-v", "error", "-i", &format!("{base}/hls/abr/master.m3u8")]))
                .wait_with_output();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success() && stderr.trim().is_empty(), "ffprobe reported errors:\n{stderr}");
    }

    drop(root);
    drop(low);
}

/// Best-effort, manual-style check with Apple's own validator: not gated
/// behind CI (no allow-listed-MUST bookkeeping here, that lives in
/// `crates/caudal/tests/support`), just evidence for the batch report. Runs
/// only when the tool is installed; prints (does not fail on) any findings
/// so the report can quote them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abr_family_apple_validator_if_present() {
    if !have("mediastreamvalidator") {
        eprintln!("SKIP: mediastreamvalidator not installed");
        return;
    }
    let fx = std::sync::Arc::new(mp4demux::demux(include_bytes!("fixtures/av.mp4")));
    let reg = Registry::new();
    let app = router(
        reg.clone(),
        HlsConfig { part_ms: 200, segment_ms: 2000, cue_tags: true, cue_out_tags: false },
        Vec::new(),
    );
    let port = free_port();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let root = reg.publish("v2main", BufferConfig::default()).unwrap();
    root.set_tracks(fx.tracks.clone()).unwrap();
    let low = reg.publish("v2main+low", BufferConfig::default()).unwrap();
    low.set_tracks(fx.tracks.clone()).unwrap();

    // The validator's own timeout (20 s) keeps polling well past a short
    // burst of frames; feed both renditions in real time, for as long as it
    // runs, so it never sees a stream that has simply gone quiet.
    fn feed(fx: std::sync::Arc<mp4demux::Demuxed>, p: caudal_core::Publisher) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let t0 = tokio::time::Instant::now();
            for n in 0..i64::MAX {
                for f in fx.looped(n) {
                    tokio::time::sleep_until(t0 + Duration::from_micros(fx.micros(&f).max(0) as u64)).await;
                    if p.push(f).is_err() {
                        return;
                    }
                }
            }
        })
    }
    let feeders = [feed(fx.clone(), root), feed(fx.clone(), low)];

    let base = format!("http://127.0.0.1:{port}");
    wait_http(&format!("{base}/hls/v2main/index.m3u8"), |b| b.contains("#EXTINF"), Duration::from_secs(10)).await;
    wait_http(&format!("{base}/hls/v2main+low/index.m3u8"), |b| b.contains("#EXTINF"), Duration::from_secs(10)).await;

    let out = Guarded::spawn(Command::new("mediastreamvalidator").args([
        "--timeout",
        "20",
        &format!("{base}/hls/v2main/master.m3u8"),
    ]))
    .wait_with_output();
    for h in feeders {
        h.abort();
    }
    let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    eprintln!("mediastreamvalidator output:\n{text}");
    // Plain HTTP still trips -50120 (no HTTP/2); that is expected and
    // unrelated to this batch (M7 territory). We only care that -50125 (no
    // rendition report) is gone now that there are two renditions.
    assert!(!text.contains("-50125"), "expected -50125 to clear with a second rendition:\n{text}");
}

/// SIGKILL to a whole process group. Never via `/bin/kill -KILL -<pgid>`:
/// Linux procps reads that as "every process of this user" (it killed the
/// GitHub runner mid-test).
fn kill_group(pgid: u32) {
    if let Some(pid) = rustix::process::Pid::from_raw(pgid as i32) {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
}
