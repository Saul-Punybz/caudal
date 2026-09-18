//! Helpers for driving the real binary with real tools (ffmpeg, Apple's
//! mediastreamvalidator). Pattern borrowed from cesbo/rsrt's test support:
//! every external tool is optional and its absence is reported, never hidden
//! behind a green test.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub fn have(tool: &str) -> bool {
    Command::new("which").arg(tool).output().is_ok_and(|o| o.status.success())
}

pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// A running `caudal` binary with its own config and ports.
pub struct Server {
    child: Child,
    pub http: u16,
    pub rtmp: u16,
    _dir: tempfile::TempDir,
}

impl Server {
    pub fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (http, rtmp) = (free_port(), free_port());
        let cfg = dir.path().join("caudal.toml");
        std::fs::write(
            &cfg,
            format!(
                "[server]\nhttp_bind = \"127.0.0.1:{http}\"\n\n[rtmp]\nbind = \"127.0.0.1:{rtmp}\"\napp = \"live\"\n\n[hls]\npart_ms = 200\nsegment_ms = 2000\n"
            ),
        )
        .unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_caudal"))
            .arg("--config")
            .arg(&cfg)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn caudal");
        let s = Self { child, http, rtmp, _dir: dir };
        s.wait_for("/healthz", Duration::from_secs(10));
        s
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{}", self.http, path)
    }

    pub fn rtmp_url(&self, name: &str) -> String {
        format!("rtmp://127.0.0.1:{}/live/{name}", self.rtmp)
    }

    pub fn get(&self, path: &str) -> Result<(u16, String), String> {
        match ureq::get(self.url(path)).call() {
            Ok(mut r) => Ok((r.status().as_u16(), r.body_mut().read_to_string().unwrap_or_default())),
            Err(ureq::Error::StatusCode(c)) => Ok((c, String::new())),
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn get_bytes(&self, path: &str) -> Vec<u8> {
        ureq::get(self.url(path)).call().unwrap().body_mut().read_to_vec().unwrap()
    }

    pub fn wait_for(&self, path: &str, timeout: Duration) {
        let t0 = Instant::now();
        while t0.elapsed() < timeout {
            if matches!(self.get(path), Ok((200, _))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("{path} never answered 200 within {timeout:?}");
    }

    /// Polls `path` until `pred` holds on the body.
    pub fn wait_until(&self, path: &str, timeout: Duration, pred: impl Fn(&str) -> bool) -> String {
        let t0 = Instant::now();
        let mut last = String::new();
        while t0.elapsed() < timeout {
            if let Ok((200, body)) = self.get(path) {
                if pred(&body) {
                    return body;
                }
                last = body;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("{path} never matched within {timeout:?}; last body:\n{last}");
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// ffmpeg publishing a synthetic test pattern (1280x720 H.264 + AAC, with
/// the wall clock burned in) over RTMP, in real time, until dropped.
pub struct Publisher {
    child: Child,
}

impl Publisher {
    pub fn rtmp(url: &str, secs: u32) -> Self {
        let child = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-re"])
            .args(["-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=30"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"])
            .args(["-vf", "drawtext=text='%{localtime\\:%H\\\\\\:%M\\\\\\:%S.%3N}':fontsize=48:x=20:y=20:fontcolor=white:box=1:boxcolor=black"])
            .args(["-t", &secs.to_string()])
            .args(["-c:v", "libx264", "-preset", "veryfast", "-tune", "zerolatency", "-g", "60", "-b:v", "2M"])
            .args(["-c:a", "aac", "-b:a", "128k"])
            .args(["-f", "flv", url])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn ffmpeg");
        Self { child }
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Runs Apple's mediastreamvalidator when available. Returns `None` when the
/// tool is not installed (Linux CI); the caller reports that, not a pass.
pub fn validate_hls(playlist_url: &str, out_dir: &Path) -> Option<Result<(), String>> {
    if !have("mediastreamvalidator") {
        return None;
    }
    let report: PathBuf = out_dir.join("validation.json");
    let out = Command::new("mediastreamvalidator")
        .args(["--timeout", "20", "--validation-data-path"])
        .arg(&report)
        .arg(playlist_url)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    let errors = text.lines().filter(|l| l.contains("ERROR")).count();
    Some(if out.status.success() && errors == 0 { Ok(()) } else { Err(text) })
}
