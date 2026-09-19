//! Golden tests for `caudal import-mist`, run through the real binary
//! against the fixtures in `tests/fixtures/mist/` (see the README there).
//!
//! Set `CAUDAL_BLESS=1` to (re)write the `.expected.toml`/`.expected.txt`
//! golden files from the binary's current output, after a deliberate
//! change to the mapping.

use std::path::{Path, PathBuf};
use std::process::Command;

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mist")
}

/// Runs `caudal import-mist <json_name> -o <tempdir>/caudal.toml`, returns
/// (report text with the "wrote <path>" line stripped, so it's stable
/// across runs, generated TOML text, the written path, the temp dir it
/// lives in — kept alive for the duration of the test).
fn run_import(json_name: &str) -> (String, String, PathBuf, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("caudal.toml");
    let output = Command::new(env!("CARGO_BIN_EXE_caudal"))
        .arg("import-mist")
        .arg(fixtures_dir().join(json_name))
        .arg("-o")
        .arg(&out)
        .output()
        .expect("run caudal import-mist");
    assert!(output.status.success(), "import-mist failed: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let report = stdout.split_once('\n').map(|(_, rest)| rest).unwrap_or("").to_string();
    let toml = std::fs::read_to_string(&out).unwrap();
    (report, toml, out, dir)
}

fn assert_golden(actual: &str, golden_path: &Path) {
    if std::env::var("CAUDAL_BLESS").is_ok() {
        std::fs::write(golden_path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(golden_path).unwrap_or_else(|e| {
        panic!("reading golden file {}: {e} (run with CAUDAL_BLESS=1 to create it)", golden_path.display())
    });
    assert_eq!(actual, expected, "golden mismatch for {}", golden_path.display());
}

fn golden_case(json_name: &str) {
    let stem = json_name.strip_suffix(".json").expect("fixture name ends in .json");
    let (report, toml, ..) = run_import(json_name);
    assert_golden(&toml, &fixtures_dir().join(format!("{stem}.expected.toml")));
    assert_golden(&report, &fixtures_dir().join(format!("{stem}.expected.txt")));
}

#[test]
fn basic_rtmp_hls_golden() {
    golden_case("basic_rtmp_hls.json");
}

#[test]
fn push_and_record_golden() {
    golden_case("push_and_record.json");
}

#[test]
fn rtsp_webrtc_files_golden() {
    golden_case("rtsp_webrtc_files.json");
}

/// Every fixture's generated `caudal.toml` must pass `caudal check` too —
/// the same validation `import-mist` already ran on itself before writing
/// the file, checked again here through the actual `check` subcommand.
#[test]
fn every_fixture_output_passes_caudal_check() {
    for json_name in ["basic_rtmp_hls.json", "push_and_record.json", "rtsp_webrtc_files.json"] {
        let (_, _, out_path, _dir) = run_import(json_name);
        let check =
            Command::new(env!("CARGO_BIN_EXE_caudal")).arg("check").arg(&out_path).output().expect("run caudal check");
        assert!(
            check.status.success(),
            "{json_name}: `caudal check` on the generated file failed: {}",
            String::from_utf8_lossy(&check.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&check.stdout).trim(), "ok");
    }
}

#[test]
fn missing_input_file_fails_without_writing_output() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("caudal.toml");
    let output = Command::new(env!("CARGO_BIN_EXE_caudal"))
        .arg("import-mist")
        .arg(dir.path().join("nonexistent.json"))
        .arg("-o")
        .arg(&out)
        .output()
        .expect("run caudal import-mist");
    assert!(!output.status.success());
    assert!(!out.exists(), "no output file should be written on failure");
}

#[test]
fn invalid_json_fails_without_writing_output() {
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.json");
    std::fs::write(&bad, "not json").unwrap();
    let out = dir.path().join("caudal.toml");
    let output = Command::new(env!("CARGO_BIN_EXE_caudal"))
        .arg("import-mist")
        .arg(&bad)
        .arg("-o")
        .arg(&out)
        .output()
        .expect("run caudal import-mist");
    assert!(!output.status.success());
    assert!(!out.exists());
}
