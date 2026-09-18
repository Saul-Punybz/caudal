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
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-hide_banner", "-loglevel", "error", "-re"])
            .args(["-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=30"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"]);
        // The burned-in clock needs ffmpeg built with freetype; Homebrew's
        // default build has no drawtext. The test does not depend on it.
        if ffmpeg_has_filter("drawtext") {
            cmd.args(["-vf", "drawtext=text='%{localtime\\:%H\\\\\\:%M\\\\\\:%S.%3N}':fontsize=48:x=20:y=20:fontcolor=white:box=1:boxcolor=black"]);
        }
        let child = cmd
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

pub fn ffmpeg_has_filter(name: &str) -> bool {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-filters"])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).lines().any(|l| l.split_whitespace().nth(1) == Some(name)))
}

/// True when CI demands the validator (`CAUDAL_REQUIRE_HLS_VALIDATOR=1`):
/// then a missing tool is a failure, not a silent skip.
pub fn validator_required() -> bool {
    std::env::var("CAUDAL_REQUIRE_HLS_VALIDATOR").is_ok_and(|v| v == "1")
}

pub fn have_validator() -> bool {
    let present = have("mediastreamvalidator");
    if !present && validator_required() {
        panic!("CAUDAL_REQUIRE_HLS_VALIDATOR=1 but mediastreamvalidator is not installed");
    }
    present
}

/// Runs Apple's mediastreamvalidator on `playlist_url`. Returns `None` when
/// the tool is absent (and not required). When `CAUDAL_VALIDATION_DIR` is
/// set, the JSON report is kept there as evidence that the tool really ran;
/// CI uploads that directory and fails if it is empty.
pub fn validate_hls(playlist_url: &str, out_dir: &Path, label: &str) -> Option<Result<(), String>> {
    if !have_validator() {
        return None;
    }
    let keep = std::env::var_os("CAUDAL_VALIDATION_DIR").map(PathBuf::from);
    let dir = keep.clone().unwrap_or_else(|| out_dir.to_path_buf());
    std::fs::create_dir_all(&dir).ok()?;
    let report = dir.join(format!("{label}.json"));
    let out = Command::new("mediastreamvalidator")
        .args(["--timeout", "20", "--validation-data-path"])
        .arg(&report)
        .arg(playlist_url)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    std::fs::write(dir.join(format!("{label}.log")), &text).ok();
    assert!(report.exists(), "mediastreamvalidator ran but wrote no report at {}", report.display());
    let errors = text.lines().filter(|l| l.contains("ERROR")).count();
    Some(if out.status.success() && errors == 0 { Ok(()) } else { Err(text) })
}

/// Serves one fixed body at `/bad.m3u8` on a local port, for canary tests.
pub fn serve_static(body: &'static str, content_type: &'static str) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        for mut c in l.incoming().flatten() {
            let mut buf = [0u8; 2048];
            let _ = c.read(&mut buf);
            let _ = write!(
                c,
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    port
}
