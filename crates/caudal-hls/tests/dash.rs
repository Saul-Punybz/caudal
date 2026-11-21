//! Real-socket check for the DASH output: a two-rendition family
//! (`dashcam`, `dashcam+low`) fed H.264 + AAC in real time, served over an
//! actual TCP port. The MPD and the init + media segments it names are
//! fetched, written out and read back by `ffprobe`; `ffmpeg` plays the live
//! MPD itself for 3 s with `-xerror` (when built with its DASH demuxer, as
//! CI's is) and GStreamer's `dashdemux` plays it in real time (when
//! installed). Skipped (with a note) when the tools are not on PATH. Shape assertions live in `src/dash_tests.rs`.

// Drives external processes through Unix process groups, like
// `multi_rendition.rs`; Windows runs its own smoke test in CI.
#![cfg(unix)]

#[path = "../src/mp4demux.rs"]
mod mp4demux;

use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use caudal_core::{BufferConfig, Registry};
use caudal_hls::{HlsConfig, router};

fn have(bin: &str) -> bool {
    Command::new("which").arg(bin).output().is_ok_and(|o| o.status.success())
}

/// Runs a tool in its own process group, killed (group and all) after
/// `limit` or on drop.
fn run(cmd: &mut Command, limit: Duration) -> Output {
    use std::os::unix::process::CommandExt;
    let child: Child = cmd.process_group(0).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("spawn tool");
    let pgid = child.id();
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = done.clone();
    let watchdog = std::thread::spawn(move || {
        let t0 = Instant::now();
        while !flag.load(std::sync::atomic::Ordering::SeqCst) {
            if t0.elapsed() > limit {
                kill_group(pgid);
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    });
    let out = child.wait_with_output().expect("wait for tool");
    done.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = watchdog.join();
    kill_group(pgid);
    out
}

/// SIGKILL to a whole process group (never via `/bin/kill -KILL -<pgid>`;
/// see `multi_rendition.rs`).
fn kill_group(pgid: u32) {
    if let Some(pid) = rustix::process::Pid::from_raw(pgid as i32) {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
}

/// Homebrew's ffmpeg is built without libxml2, so it has no DASH demuxer;
/// the CI build (BtbN, GPL) has one.
fn ffmpeg_has_dash() -> bool {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-demuxers"])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).lines().any(|l| l.split_whitespace().nth(1) == Some("dash")))
}

fn gst_has_dash() -> bool {
    have("gst-launch-1.0")
        && Command::new("gst-inspect-1.0").arg("dashdemux").output().is_ok_and(|o| o.status.success())
}

fn get(url: &str) -> Option<(String, Vec<u8>)> {
    let mut r = ureq::get(url).call().ok()?;
    let ct = r.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
    Some((ct, r.body_mut().with_config().limit(64 << 20).read_to_vec().ok()?))
}

fn attr<'a>(tag: &'a str, name: &str) -> &'a str {
    let key = format!(" {name}=\"");
    let at = tag.find(&key).unwrap_or_else(|| panic!("no {name} in {tag}")) + key.len();
    &tag[at..at + tag[at..].find('"').unwrap()]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dash_family_plays_in_real_players() {
    if !have("ffmpeg") || !have("ffprobe") {
        eprintln!("SKIP: ffmpeg/ffprobe not on PATH");
        return;
    }
    let fx = Arc::new(mp4demux::demux(include_bytes!("fixtures/av.mp4")));
    let reg = Registry::new();
    let app = router(reg.clone(), HlsConfig { ts: false, ..HlsConfig::default() }, Vec::new());
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let root = reg.publish("dashcam", BufferConfig::default()).unwrap();
    root.set_tracks(fx.tracks.clone()).unwrap();
    let low = reg.publish("dashcam+low", BufferConfig::default()).unwrap();
    low.set_tracks(fx.tracks.clone()).unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    // Real time, for as long as the test runs: ffmpeg reads a live MPD.
    let feeder = {
        let fx = fx.clone();
        tokio::spawn(async move {
            let t0 = tokio::time::Instant::now();
            for n in 0..i64::MAX {
                for f in fx.looped(n) {
                    tokio::time::sleep_until(t0 + Duration::from_micros(fx.micros(&f).max(0) as u64)).await;
                    if root.push(f.clone()).is_err() || low.push(f).is_err() {
                        return;
                    }
                }
            }
        })
    };

    let base = format!("http://127.0.0.1:{port}");
    let mpd_url = format!("{base}/hls/dashcam/manifest.mpd");
    // Three complete segments in both renditions.
    let t0 = Instant::now();
    let mpd = loop {
        let body = tokio::task::spawn_blocking({
            let u = mpd_url.clone();
            move || get(&u)
        })
        .await
        .unwrap();
        if let Some((ct, b)) = body {
            let text = String::from_utf8(b).unwrap();
            assert_eq!(ct, "application/dash+xml");
            if text.matches("<Representation ").count() == 3 && text.matches("<S ").count() >= 8 {
                break text;
            }
        }
        assert!(t0.elapsed() < Duration::from_secs(30), "MPD never filled");
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    eprintln!("{mpd}");

    let dir = std::env::temp_dir().join(format!("caudal-dash-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mpd_file = dir.join("manifest.mpd");
    std::fs::write(&mpd_file, &mpd).unwrap();
    if have("xmllint") {
        let out = run(Command::new("xmllint").arg("--noout").arg(&mpd_file), Duration::from_secs(20));
        assert!(out.status.success(), "xmllint: {}", String::from_utf8_lossy(&out.stderr));
    }

    // Every Representation: its init segment plus its first three media
    // segments, concatenated into one file, read back by ffprobe.
    let mut probed = Vec::new();
    for rep in mpd.split("<Representation ").skip(1) {
        let rep = format!(" {rep}");
        let rep = rep.as_str();
        let id = attr(rep, "id").to_owned();
        let tpl = &rep[rep.find("<SegmentTemplate").unwrap()..];
        let start: u64 = attr(tpl, "startNumber").parse().unwrap();
        let resolve = |uri: &str| format!("{base}/hls/{}", uri.strip_prefix("../").unwrap());
        let init_url = resolve(attr(tpl, "initialization"));
        let media = attr(tpl, "media").to_owned();
        let (urls, file) = (
            std::iter::once(init_url)
                .chain((start..start + 3).map(|n| resolve(&media.replace("$Number$", &n.to_string()))))
                .collect::<Vec<_>>(),
            dir.join(format!("{}.mp4", id.replace('+', "_"))),
        );
        let bytes = tokio::task::spawn_blocking(move || {
            let mut all = Vec::new();
            for u in urls {
                all.extend(get(&u).unwrap_or_else(|| panic!("GET {u}")).1);
            }
            all
        })
        .await
        .unwrap();
        std::fs::write(&file, bytes).unwrap();
        let out = run(
            Command::new("ffprobe")
                .args(["-v", "error", "-show_entries", "stream=codec_name,codec_type", "-of", "csv=p=0"])
                .arg(&file),
            Duration::from_secs(30),
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success() && stderr.trim().is_empty(), "ffprobe {id}: {stderr}");
        probed.push(String::from_utf8_lossy(&out.stdout).trim().to_owned());
        let out = run(
            Command::new("ffmpeg").args(["-v", "error", "-xerror", "-i"]).arg(&file).args(["-f", "null", "-"]),
            Duration::from_secs(30),
        );
        assert!(out.status.success(), "ffmpeg decode {id}: {}", String::from_utf8_lossy(&out.stderr));
    }
    probed.sort();
    assert_eq!(probed, ["aac,audio", "h264,video", "h264,video"], "one track per Representation");

    let mut players = 0;
    if ffmpeg_has_dash() {
        // ffprobe sees every Representation of the live MPD.
        let out = run(
            Command::new("ffprobe").args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_type",
                "-of",
                "csv=p=0",
                &mpd_url,
            ]),
            Duration::from_secs(60),
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success() && stderr.trim().is_empty(), "ffprobe MPD: {stderr}");
        let mut kinds: Vec<_> = String::from_utf8_lossy(&out.stdout).lines().map(str::to_owned).collect();
        kinds.sort();
        assert_eq!(kinds, ["audio", "video", "video"]);

        // ffmpeg plays 3 s of the live MPD, stopping on the first error.
        let out = run(
            Command::new("ffmpeg").args(["-v", "error", "-xerror", "-i", &mpd_url, "-t", "3", "-f", "null", "-"]),
            Duration::from_secs(90),
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "ffmpeg -xerror on the live MPD failed:\n{stderr}");
        eprintln!("ffmpeg -xerror on the live MPD: ok");
        players += 1;
    } else {
        eprintln!("SKIP: this ffmpeg has no dash demuxer (built without libxml2)");
    }
    if gst_has_dash() {
        // GStreamer's dashdemux plays the live MPD in real time for 8 s
        // (clock-synced fake sinks); every buffer reaching a sink is logged.
        let sink = |n: &str| format!("fakesink name={n} sync=true silent=false");
        let out = run(
            Command::new("gst-launch-1.0").args([
                "-v",
                "playbin",
                &format!("uri={mpd_url}"),
                &format!("video-sink={}", sink("vs")),
                &format!("audio-sink={}", sink("as")),
            ]),
            Duration::from_secs(8),
        );
        let log = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
        assert!(!log.contains("ERROR:"), "GStreamer reported an error:\n{log}");
        let frames = |n: &str| log.matches(&format!("GstFakeSink:{n}: last-message = chain")).count();
        let (video, audio) = (frames("vs"), frames("as"));
        assert!(video >= 90 && audio >= 90, "GStreamer rendered too little: {video} video, {audio} audio buffers");
        eprintln!("GStreamer playbin on the live MPD: {video} video, {audio} audio buffers in 8 s");
        players += 1;
    } else {
        eprintln!("SKIP: GStreamer dashdemux not installed");
    }
    assert!(players > 0 || std::env::var_os("CI").is_none(), "CI must play the MPD with at least one player");

    feeder.abort();
    let _ = std::fs::remove_dir_all(&dir);
}
