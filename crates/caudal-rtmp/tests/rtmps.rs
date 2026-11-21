//! RTMPS publish: real `ffmpeg` publishing `rtmps://` to `caudal_rtmp::serve_tls`
//! with a self-signed certificate generated here, through the same
//! `caudal_tls::tls_acceptor` the server uses for `[rtmp] tls_bind`.
//!
//! ffmpeg verifies the certificate (`-tls_verify 1 -ca_file`), so a pass
//! means the listener presented the configured certificate, not just "some
//! TLS". Every test checks for `ffmpeg` first and prints "SKIP" without it.
#![cfg(unix)]

use std::net::TcpListener as StdTcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use caudal_core::{BufferConfig, Codec, Registry, TrackKind};
use caudal_rtmp::{RtmpConfig, serve, serve_tls};
use caudal_tls::CertSource;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn have_ffmpeg() -> bool {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-protocols"])
        .output()
        .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains("rtmps"))
}

fn free_port() -> u16 {
    StdTcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn write_cert(dir: &Path, name: &str) -> (PathBuf, PathBuf) {
    let certified = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string(), "localhost".to_string()])
        .expect("generate self-signed cert");
    let cert = dir.join(format!("{name}.cert.pem"));
    let key = dir.join(format!("{name}.key.pem"));
    std::fs::write(&cert, certified.cert.pem()).unwrap();
    std::fs::write(&key, certified.key_pair.serialize_pem()).unwrap();
    (cert, key)
}

async fn wait_for<T>(timeout: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(v) = f() {
            return Some(v);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A plain RTMP listener and an RTMPS listener on one registry, as
/// `[rtmp] bind` + `tls_bind` run in the server.
struct Servers {
    registry: Arc<Registry>,
    plain: u16,
    tls: u16,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Servers {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

async fn start(cert: &Path, key: &Path) -> Servers {
    let registry = Registry::new();
    let source = CertSource::Files { cert: cert.to_path_buf(), key: key.to_path_buf() };
    for _ in 0..5 {
        let (plain, tls) = (free_port(), free_port());
        let cfg = |port: u16| RtmpConfig {
            bind: format!("127.0.0.1:{port}").parse().unwrap(),
            app: "live".into(),
            buffer: BufferConfig::default(),
        };
        let acceptor = caudal_tls::tls_acceptor(&source).await.expect("tls acceptor");
        let (reg_a, reg_b) = (registry.clone(), registry.clone());
        let (cfg_a, cfg_b) = (cfg(plain), cfg(tls));
        let tasks = vec![
            tokio::spawn(async move {
                let _ = serve(cfg_a, reg_a).await;
            }),
            tokio::spawn(async move {
                let _ = serve_tls(cfg_b, acceptor, reg_b).await;
            }),
        ];
        let up = wait_for(Duration::from_secs(2), || {
            let a = std::net::TcpStream::connect(("127.0.0.1", plain)).ok()?;
            let b = std::net::TcpStream::connect(("127.0.0.1", tls)).ok()?;
            Some((a, b))
        })
        .await;
        if up.is_some() && tasks.iter().all(|t| !t.is_finished()) {
            return Servers { registry, plain, tls, tasks };
        }
        for t in &tasks {
            t.abort();
        }
    }
    panic!("could not bind two free ports in 5 tries");
}

/// Publishes 4 s of H.264 + AAC test signal to `url`; `extra` goes before
/// the output (TLS options).
fn spawn_publish(url: &str, extra: &[&str]) -> std::process::Child {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-re"])
        .args(["-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"])
        .args(["-c:v", "libx264", "-g", "60", "-c:a", "aac", "-t", "4"])
        .args(extra)
        .args(["-f", "flv", url])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ffmpeg")
}

async fn assert_h264_aac(registry: &Registry, name: &str) {
    let stream = wait_for(Duration::from_secs(10), || registry.get(name))
        .await
        .unwrap_or_else(|| panic!("stream {name:?} never published"));
    let tracks = wait_for(Duration::from_secs(5), || {
        let t = stream.tracks();
        (t.len() == 2).then_some(t)
    })
    .await
    .unwrap_or_else(|| panic!("{name}: expected a video and an audio track, got {:?}", stream.tracks()));
    let video = tracks.iter().find(|t| t.kind() == TrackKind::Video).expect("video track");
    let audio = tracks.iter().find(|t| t.kind() == TrackKind::Audio).expect("audio track");
    assert_eq!(video.codec, Codec::H264);
    assert_eq!(audio.codec, Codec::Aac);
    let vp = video.video.expect("video params");
    assert_eq!((vp.width, vp.height), (640, 360));
    wait_for(Duration::from_secs(6), || (stream.stats().frames_in > 30).then_some(()))
        .await
        .unwrap_or_else(|| panic!("{name}: frames never flowed"));
}

async fn wait_exit(child: &mut std::process::Child, timeout: Duration) -> (std::process::ExitStatus, String) {
    let status = wait_for(timeout, || child.try_wait().ok().flatten()).await;
    let status = match status {
        Some(s) => s,
        None => {
            let _ = child.kill();
            child.wait().unwrap()
        }
    };
    let mut err = String::new();
    if let Some(mut e) = child.stderr.take() {
        use std::io::Read;
        let _ = e.read_to_string(&mut err);
    }
    (status, err)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rtmps_and_rtmp_publish_side_by_side() {
    if !have_ffmpeg() {
        eprintln!("SKIP: ffmpeg with rtmps not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = write_cert(dir.path(), "server");
    let s = start(&cert, &key).await;

    // RTMPS, certificate verified against the generated CA.
    let ca = cert.to_str().unwrap();
    let mut secure =
        spawn_publish(&format!("rtmps://127.0.0.1:{}/live/secure", s.tls), &["-tls_verify", "1", "-ca_file", ca]);
    // Plain RTMP on the other port, at the same time.
    let mut plain = spawn_publish(&format!("rtmp://127.0.0.1:{}/live/plain", s.plain), &[]);

    assert_h264_aac(&s.registry, "secure").await;
    assert_h264_aac(&s.registry, "plain").await;

    let (st, err) = wait_exit(&mut secure, Duration::from_secs(15)).await;
    assert!(st.success(), "rtmps publish failed: {err}");
    let (st, err) = wait_exit(&mut plain, Duration::from_secs(15)).await;
    assert!(st.success(), "rtmp publish failed: {err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_tls_is_rejected() {
    if !have_ffmpeg() {
        eprintln!("SKIP: ffmpeg with rtmps not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = write_cert(dir.path(), "server");
    let (other_ca, _) = write_cert(dir.path(), "other");
    let s = start(&cert, &key).await;

    // A client that trusts a different CA refuses the handshake.
    let mut wrong_ca = spawn_publish(
        &format!("rtmps://127.0.0.1:{}/live/wrongca", s.tls),
        &["-tls_verify", "1", "-ca_file", other_ca.to_str().unwrap()],
    );
    let (st, _) = wait_exit(&mut wrong_ca, Duration::from_secs(15)).await;
    assert!(!st.success(), "ffmpeg accepted a certificate from an untrusted CA");

    // Clear-text RTMP sent to the TLS port is not a ClientHello: no publish.
    let mut cleartext = spawn_publish(&format!("rtmp://127.0.0.1:{}/live/cleartext", s.tls), &[]);
    let (st, _) = wait_exit(&mut cleartext, Duration::from_secs(15)).await;
    assert!(!st.success(), "clear-text RTMP was accepted on the RTMPS port");

    // Raw garbage: the server closes the connection instead of hanging.
    let mut tcp = tokio::net::TcpStream::connect(("127.0.0.1", s.tls)).await.unwrap();
    tcp.write_all(b"\x03not a tls client hello at all, just bytes\r\n\r\n").await.unwrap();
    let mut buf = [0u8; 256];
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match tcp.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(_) => continue, // a TLS alert, then EOF
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "server kept a garbage connection open");

    for name in ["wrongca", "cleartext"] {
        assert!(s.registry.get(name).is_none(), "{name} must not have published");
    }
}

/// `[tls]` certificate files replaced on disk: the RTMPS listener presents
/// the new one to the next connection without a restart (5 s poll).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn certificate_files_reload_without_restart() {
    if !have_ffmpeg() {
        eprintln!("SKIP: ffmpeg with rtmps not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = write_cert(dir.path(), "server");
    let s = start(&cert, &key).await;

    // Swap in a new pair; keep a copy of the new cert as the CA to trust.
    std::thread::sleep(Duration::from_millis(1100)); // distinct mtime
    let (new_cert, new_key) = write_cert(dir.path(), "renewed");
    std::fs::copy(&new_cert, &cert).unwrap();
    std::fs::copy(&new_key, &key).unwrap();

    let url = format!("rtmps://127.0.0.1:{}/live/renewed", s.tls);
    let ca = new_cert.to_str().unwrap().to_owned();
    // Up to ~8 s for the 5 s poller; each attempt is a short publish.
    let ok = wait_for_async(Duration::from_secs(12), || {
        let mut child = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error"])
            .args(["-f", "lavfi", "-i", "testsrc2=size=320x240:rate=10", "-c:v", "libx264", "-t", "0.5"])
            .args(["-tls_verify", "1", "-ca_file", &ca, "-f", "flv", &url])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child.wait().is_ok_and(|s| s.success())
    })
    .await;
    assert!(ok, "the reloaded certificate was never presented");
}

async fn wait_for_async(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if tokio::task::block_in_place(&mut f) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}
