//! The real binary: refuses to start open on a public address, and
//! `caudal hash-password` output is accepted by `caudal check`. No ports
//! are bound: the refusal happens before any listener starts.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn caudal() -> Command {
    Command::new(env!("CARGO_BIN_EXE_caudal"))
}

#[test]
fn refuses_to_start_without_login_on_a_public_address() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("caudal.toml");
    std::fs::write(&cfg, "[server]\nhttp_bind = \"0.0.0.0:0\"\n").unwrap();
    let mut child = caudal().arg("--config").arg(&cfg).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let t0 = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if t0.elapsed() > Duration::from_secs(20) {
            let _ = child.kill();
            panic!("caudal kept running on 0.0.0.0 with no [admin]");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let out = child.wait_with_output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(!status.success());
    assert!(text.contains("refusing to start"), "{text}");
}

#[test]
fn hash_password_output_passes_config_check() {
    let mut child = caudal().arg("hash-password").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(b"a long enough password\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let phc = String::from_utf8(out.stdout).unwrap().trim().to_owned();
    assert!(phc.starts_with("$argon2id$v=19$"), "{phc}");

    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("caudal.toml");
    std::fs::write(&cfg, format!("[[admin.users]]\nname = \"ana\"\npassword_hash = \"{phc}\"\n")).unwrap();
    let check = caudal().arg("check").arg(&cfg).output().unwrap();
    assert!(check.status.success(), "{}", String::from_utf8_lossy(&check.stderr));

    // Too short is refused.
    let mut child = caudal()
        .arg("hash-password")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"short\n").unwrap();
    assert!(!child.wait_with_output().unwrap().status.success());
}
