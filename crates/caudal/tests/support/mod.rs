//! Helpers for driving the real binary with real tools (ffmpeg, Apple's
//! mediastreamvalidator). Pattern borrowed from cesbo/rsrt's test support:
//! every external tool is optional and its absence is reported, never hidden
//! behind a green test.
//!
//! Shared by every `tests/*.rs` integration binary, each compiled (and
//! dead-code-checked) on its own: a helper only one of them calls is not
//! dead code overall, just unused in whichever binary skips it.
#![allow(dead_code)]

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub fn have(tool: &str) -> bool {
    Command::new("which").arg(tool).output().is_ok_and(|o| o.status.success())
}

pub fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// A running `caudal` binary with its own config and ports.
pub struct Server {
    child: Child,
    pub http: u16,
    pub rtmp: u16,
    pub srt: u16,
    /// The WebRTC UDP port (ICE); WHIP/WHEP signaling goes over `http`.
    #[allow(dead_code)]
    pub webrtc: u16,
    /// The config file path it was started with; rewrite it and call
    /// [`Server::reload_via_api`] or [`Server::sighup`] to test hot reload.
    pub cfg_path: PathBuf,
    _dir: tempfile::TempDir,
}

impl Server {
    pub fn start() -> Self {
        Self::start_with("")
    }

    /// Starts with `extra` TOML appended (new sections only). `{dir}` in it
    /// is replaced with the server's temp directory.
    pub fn start_with(extra: &str) -> Self {
        // Ports are probed and released, so another process (or this
        // server's own next probe) can take one before caudal binds it; the
        // server then exits and /healthz never answers (CI, 19 Sep 2026:
        // reload_via_sighup_also_applies). Five distinct ports, and a fresh
        // set if the server dies or stays silent.
        let mut last_err = String::new();
        for attempt in 1..=3 {
            match Self::try_start(extra) {
                Ok(s) => return s,
                Err(e) => {
                    eprintln!("caudal start attempt {attempt} failed: {e}; retrying with new ports");
                    last_err = e;
                }
            }
        }
        panic!("caudal did not start in 3 attempts: {last_err}");
    }

    fn try_start(extra: &str) -> Result<Self, String> {
        let dir = tempfile::tempdir().unwrap();
        let mut ports: Vec<u16> = Vec::new();
        while ports.len() < 5 {
            let p = if ports.len() < 2 { free_port() } else { free_udp_port() };
            if !ports.contains(&p) {
                ports.push(p);
            }
        }
        let (http, rtmp, srt, webrtc, moq) = (ports[0], ports[1], ports[2], ports[3], ports[4]);
        let cfg = dir.path().join("caudal.toml");
        std::fs::write(
            &cfg,
            format!(
                "[server]\nhttp_bind = \"127.0.0.1:{http}\"\n\n[rtmp]\nbind = \"127.0.0.1:{rtmp}\"\napp = \"live\"\n\n[srt]\nbind = \"127.0.0.1:{srt}\"\n\n[webrtc]\nudp_bind = \"127.0.0.1:{webrtc}\"\n\n[moq]\nbind = \"127.0.0.1:{moq}\"\n\n[hls]\npart_ms = 200\nsegment_ms = 2000\n{}"
                , extra.replace("{dir}", &dir.path().display().to_string())
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
        let mut s = Self { child, http, rtmp, srt, webrtc, cfg_path: cfg, _dir: dir };
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(10) {
            if matches!(s.get("/healthz"), Ok((200, _))) {
                return Ok(s);
            }
            if let Ok(Some(status)) = s.child.try_wait() {
                return Err(format!("caudal exited during startup ({status}), ports {ports:?}"));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(format!("/healthz never answered 200 within 10s, ports {ports:?}"))
        // `s` drops here: Drop kills the child.
    }

    /// Overwrites the config file at [`Server::cfg_path`] with `text`; does
    /// not itself trigger a reload (see [`Server::reload_via_api`] /
    /// [`Server::sighup`]).
    pub fn write_config(&self, text: &str) {
        std::fs::write(&self.cfg_path, text).unwrap();
    }

    /// `POST /api/v1/config/reload`: `(status, body)`. Reads the body on a
    /// 400 too (`http_status_as_error(false)`): that's where the
    /// rejection's error message lives.
    pub fn reload_via_api(&self) -> (u16, String) {
        let res =
            ureq::post(self.url("/api/v1/config/reload")).config().http_status_as_error(false).build().send_empty();
        match res {
            Ok(mut r) => (r.status().as_u16(), r.body_mut().read_to_string().unwrap_or_default()),
            Err(e) => panic!("POST /api/v1/config/reload: {e}"),
        }
    }

    /// Sends SIGHUP to the running process, the other way to trigger a
    /// reload (`kill -HUP <pid>`).
    pub fn sighup(&self) {
        let status = Command::new("kill").args(["-HUP", &self.child.id().to_string()]).status().unwrap();
        assert!(status.success(), "kill -HUP failed");
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{}", self.http, path)
    }

    pub fn srt_url(&self, name: &str) -> String {
        format!("srt://127.0.0.1:{}?streamid=publish/{name}", self.srt)
    }

    pub fn whip_url(&self, name: &str) -> String {
        format!("http://127.0.0.1:{}/whip/{name}", self.http)
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

impl Publisher {
    /// The same test pattern as MPEG-TS over SRT. Homebrew's ffmpeg has no
    /// SRT protocol, so ffmpeg writes TS to a pipe and libsrt's
    /// srt-live-transmit carries it. Both run in their own process group so
    /// dropping the publisher stops the whole pipeline.
    pub fn srt(url: &str, secs: u32) -> Self {
        use std::os::unix::process::CommandExt;
        let pipeline = format!(
            "exec ffmpeg -hide_banner -loglevel error -re -f lavfi -i testsrc2=size=1280x720:rate=30 \
             -f lavfi -i sine=frequency=440:sample_rate=48000 -t {secs} \
             -c:v libx264 -preset veryfast -tune zerolatency -g 60 -b:v 2M -c:a aac -b:a 128k -f mpegts - \
             | srt-live-transmit -q -chunk:1316 file://con '{url}'"
        );
        let child = Command::new("sh")
            .args(["-c", &pipeline])
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn ffmpeg | srt-live-transmit");
        Self { child }
    }
}

impl Publisher {
    /// The test pattern as H.264 (no B-frames: WebRTC browsers cannot decode
    /// them) + Opus over WHIP, using ffmpeg's WHIP muxer (ffmpeg >= 8).
    pub fn whip(url: &str, secs: u32) -> Self {
        let child = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-re"])
            .args(["-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=30"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"])
            .args(["-t", &secs.to_string()])
            .args([
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-tune",
                "zerolatency",
                "-profile:v",
                "baseline",
                "-bf",
                "0",
                "-g",
                "60",
                "-b:v",
                "2M",
            ])
            .args(["-c:a", "libopus", "-b:a", "96k", "-ar", "48000", "-ac", "2"])
            .args(["-f", "whip", url])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn ffmpeg whip");
        Self { child }
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        // Kill the whole process group (the SRT pipeline has two processes).
        // SIGKILL, not SIGTERM: srt-live-transmit outlived SIGTERM and kept
        // the test's stderr open (nextest reported the test as leaky).
        // A direct killpg: Linux procps `/bin/kill -KILL -<pgid>` signals
        // every process of the user (it killed the GitHub runner).
        if let Some(pid) = rustix::process::Pid::from_raw(self.child.id() as i32) {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
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
    let blocking = validator_blocking_issues(&text);
    Some(if blocking.is_empty() { Ok(()) } else { Err(format!("{}\n\n{text}", blocking.join("\n"))) })
}

/// MUST-level issues that are known and scheduled, each with the milestone
/// that removes it. Keep this list short and dated.
const KNOWN_MUST: &[(&str, &str)] = &[
    // Apple requires HTTP/2 for LL-HLS; in practice that needs TLS + ALPN h2.
    // Removed by M7 (TLS/ACME). Added 18 Sep 2026.
    ("-50120", "Content not delivered via HTTP/2"),
    // With a single rendition Apple's validator demands EXT-X-RENDITION-REPORT
    // yet rejects a report that references the playlist itself (-50099;
    // tried 18 Sep 2026, also with a multivariant playlist in front). Only a
    // second rendition satisfies it: removed by ABR (M11) or demuxed audio.
    ("-50125", "Low-latency playlist MUST declare EXT-X-RENDITION-REPORT tags"),
];

/// Issue lines under the "CRITICAL Errors" and "MUST Fix HLS Spec Issues"
/// sections of mediastreamvalidator's output, minus `KNOWN_MUST`. The tool
/// exits 0 even with critical errors and never prints the word "ERROR", so
/// the exit code alone proves nothing (the canary test caught this).
pub fn validator_blocking_issues(text: &str) -> Vec<String> {
    let mut section = "";
    let mut found = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if t.ends_with("CRITICAL Errors") || t.starts_with("MUST Fix") {
            section = "blocking";
        } else if t.starts_with("SHOULD Fix") || t == "CAUTION" || t.ends_with("Summary") {
            section = "";
        } else if section == "blocking" && t.starts_with('-') && t.contains(':') && !t.starts_with("---") {
            let code = t.split(':').next().unwrap_or("");
            if !KNOWN_MUST.iter().any(|(c, _)| *c == code) {
                found.push(t.to_string());
            }
        }
    }
    found
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

/// Speech for the captions tests: `text` spoken in `lang` into a WAV at
/// `path`, by macOS `say` (voices Paulina / Samantha) or `espeak-ng`.
/// Returns which one, or `None` if neither is installed.
pub fn speak(lang: &str, text: &str, path: &Path) -> Option<&'static str> {
    if have("say") {
        let voice = say_voice(lang);
        let ok = Command::new("say")
            .args(["-v", &voice, "-o"])
            .arg(path)
            .args(["--data-format=LEI16@16000", text])
            .status()
            .is_ok_and(|s| s.success());
        return ok.then_some("say");
    }
    if have("espeak-ng") {
        let ok = Command::new("espeak-ng")
            .args(["-v", lang, "-s", "150", "-w"])
            .arg(path)
            .arg(text)
            .status()
            .is_ok_and(|s| s.success());
        return ok.then_some("espeak-ng");
    }
    None
}

impl Publisher {
    /// The test pattern with `wav` as its audio (then silence), AAC 48 kHz,
    /// over RTMP in real time.
    pub fn rtmp_with_audio(url: &str, wav: &Path, secs: u32) -> Self {
        let child = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-re"])
            .args(["-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30"])
            .arg("-i")
            .arg(wav)
            .args(["-af", "apad", "-t", &secs.to_string()])
            .args(["-c:v", "libx264", "-preset", "veryfast", "-tune", "zerolatency", "-g", "60", "-b:v", "1M"])
            .args(["-c:a", "aac", "-ar", "48000", "-ac", "2", "-b:a", "128k"])
            .args(["-f", "flv", url])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn ffmpeg");
        Self { child }
    }
}

/// A `say` voice for `lang`: Paulina (es_MX) / Samantha (en_US) when
/// installed, else the first installed voice of that language (CI runners
/// do not always have the same voices).
fn say_voice(lang: &str) -> String {
    let want = if lang == "es" { "Paulina" } else { "Samantha" };
    let list = Command::new("say").args(["-v", "?"]).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
    let list = list.unwrap_or_default();
    let installed: Vec<(&str, &str)> = list
        .lines()
        .filter_map(|l| {
            let (name, rest) = l.split_once("  ")?;
            Some((name.trim(), rest.split_whitespace().next()?))
        })
        .collect();
    if installed.iter().any(|(n, _)| *n == want) {
        return want.to_owned();
    }
    installed
        .iter()
        .find(|(_, locale)| locale.starts_with(&format!("{lang}_")))
        .map_or_else(|| want.to_owned(), |(n, _)| (*n).to_owned())
}
