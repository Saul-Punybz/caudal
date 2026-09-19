//! `[captions]`: automatic live captions (`caudal-captions`), their
//! `/metrics` lines, and `caudal captions fetch-model`.

use std::fmt::Write as _;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// `[captions]` with one or more `[[captions.stream]]`. Off unless a
/// stream rule is present.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct CaptionsSection {
    /// Where models live: `<model_dir>/whisper-<model>/`.
    pub model_dir: PathBuf,
    /// `tiny`, `base` or `small` (what `fetch-model` downloads), or the
    /// name of another `whisper-<name>` directory under `model_dir`.
    pub model: String,
    /// Inference threads: the most cores captioning may use, all streams
    /// together.
    pub threads: usize,
    /// `auto` (Metal on macOS, else CPU), `cpu` or `metal`.
    pub device: String,
    /// Streams captioned at once.
    pub max_streams: usize,
    pub stream: Vec<CaptionsStreamEntry>,
}

impl Default for CaptionsSection {
    fn default() -> Self {
        Self {
            model_dir: "models".into(),
            model: "base".into(),
            threads: 2,
            // CPU by default: a GPU driver abort (below Rust) would take the
            // whole server down; tiny/base still run 4-8x faster than real
            // time on CPU (docs/research/CAPTIONS.md). Metal is opt-in.
            device: "cpu".into(),
            max_streams: 2,
            stream: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CaptionsStreamEntry {
    /// Stream names or `prefix*` patterns.
    pub streams: Vec<String>,
    /// `es`, `en` (any language code the model knows) or `auto`.
    pub language: String,
}

impl CaptionsSection {
    /// The runtime config, or `None` when no stream is captioned.
    /// `segment_ms` is `[hls] segment_ms`: the last cue of each chunk stays
    /// up a segment and a second, so a player that loads the subtitle
    /// segment late still shows it.
    pub fn to_runtime(&self, segment_ms: u32) -> Result<Option<caudal_captions::CaptionsConfig>, String> {
        if self.stream.is_empty() {
            return Ok(None);
        }
        if self.threads == 0 || self.threads > 64 {
            return Err(format!("[captions] threads = {} (1 to 64)", self.threads));
        }
        if self.max_streams == 0 {
            return Err("[captions] max_streams must be at least 1".into());
        }
        if self.model.is_empty()
            || !self.model.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
        {
            return Err(format!("[captions] model `{}` is not a plain name (tiny, base, small)", self.model));
        }
        let device = match self.device.as_str() {
            "auto" => caudal_captions::DeviceChoice::Auto,
            "cpu" => caudal_captions::DeviceChoice::Cpu,
            "metal" => caudal_captions::DeviceChoice::Metal,
            other => return Err(format!("[captions] device `{other}` (auto, cpu or metal)")),
        };
        let mut rules = Vec::new();
        for s in &self.stream {
            if s.streams.is_empty() {
                return Err("[[captions.stream]] needs at least one entry in `streams`".into());
            }
            let language = match s.language.as_str() {
                "auto" => caudal_captions::Language::Auto,
                code if (2..=3).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_lowercase()) => {
                    caudal_captions::Language::Fixed(code.to_owned())
                }
                other => return Err(format!("[[captions.stream]] language `{other}` (es, en or auto)")),
            };
            rules.push(caudal_captions::StreamRule { streams: s.streams.clone(), language });
        }
        Ok(Some(caudal_captions::CaptionsConfig {
            model_dir: self.model_dir.clone(),
            model: self.model.clone(),
            threads: self.threads,
            device,
            max_streams: self.max_streams,
            min_display_ms: segment_ms.saturating_add(1000),
            rules,
        }))
    }
}

/// One per-stream metric: name, type, help, value.
type Series = (&'static str, &'static str, &'static str, fn(&caudal_captions::StreamMetrics) -> f64);

/// Prometheus lines for captioned streams.
pub fn render_metrics(c: &caudal_captions::Captions) -> String {
    let m = c.metrics();
    let mut out = String::new();
    let _ = writeln!(out, "# HELP caudal_captions_model_ready 1 once the speech-to-text model is loaded.");
    let _ = writeln!(out, "# TYPE caudal_captions_model_ready gauge");
    let _ = writeln!(out, "caudal_captions_model_ready {}", u8::from(c.model_ready()));
    let _ = writeln!(out, "# HELP caudal_captions_engine_panics_total Inference panics caught and recovered from.");
    let _ = writeln!(out, "# TYPE caudal_captions_engine_panics_total counter");
    let _ = writeln!(out, "caudal_captions_engine_panics_total {}", c.engine_panics());
    let series: [Series; 8] = [
        ("caudal_captions_chunks_total", "counter", "Audio chunks transcribed.", |s| s.chunks as f64),
        (
            "caudal_captions_inferences_total",
            "counter",
            "Inference runs (chunks queued while the engine was busy share one).",
            |s| s.inferences as f64,
        ),
        ("caudal_captions_cues_total", "counter", "Caption cues produced.", |s| s.cues as f64),
        (
            "caudal_captions_dropped_audio_seconds_total",
            "counter",
            "Audio not captioned because transcription fell behind.",
            |s| s.dropped_audio_seconds,
        ),
        ("caudal_captions_dropped_chunks_total", "counter", "Chunks dropped because transcription fell behind.", |s| {
            s.dropped_chunks as f64
        }),
        (
            "caudal_captions_real_time_factor",
            "gauge",
            "Inference time / audio time, everything transcribed so far.",
            |s| s.real_time_factor,
        ),
        (
            "caudal_captions_last_real_time_factor",
            "gauge",
            "Inference time / audio time of the last inference run.",
            |s| s.last_real_time_factor,
        ),
        ("caudal_captions_latency_seconds", "gauge", "Media time from the end of speech to its caption.", |s| {
            s.latency_seconds
        }),
    ];
    for (name, kind, help, get) in series {
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} {kind}");
        for s in &m {
            let _ = writeln!(out, "{name}{{stream=\"{}\"}} {}", crate::metrics::escape(&s.stream), get(s));
        }
    }
    out
}

/// `caudal captions fetch-model <name> --dir <dir>`: downloads a pinned
/// Whisper model from Hugging Face, checks every file's size and SHA-256,
/// and only then moves it into place.
pub fn fetch_model_cmd(name: &str, dir: &Path) -> ExitCode {
    let Some(model) = caudal_captions::models::known(name) else {
        let names: Vec<&str> = caudal_captions::models::KNOWN.iter().map(|m| m.name).collect();
        eprintln!("unknown model `{name}`; one of: {}", names.join(", "));
        return ExitCode::FAILURE;
    };
    let dest = caudal_captions::models::model_path(dir, name);
    if caudal_captions::models::check(dir, name).is_ok() {
        println!("{} already present ({} MB)", dest.display(), model.total_size() / 1_000_000);
        return ExitCode::SUCCESS;
    }
    if let Err(e) = std::fs::create_dir_all(&dest) {
        eprintln!("creating {}: {e}", dest.display());
        return ExitCode::FAILURE;
    }
    println!(
        "fetching whisper-{name} ({} MB) from {} @ {}",
        model.total_size() / 1_000_000,
        model.repo,
        &model.revision[..8]
    );
    for f in &model.files {
        let path = dest.join(f.name);
        if std::fs::metadata(&path).is_ok_and(|m| m.len() == f.size) {
            continue;
        }
        if let Err(e) = download(&model.url(f.name), &path, f.size, f.sha256) {
            eprintln!("{}: {e}", f.name);
            return ExitCode::FAILURE;
        }
        println!("  {} ok ({} bytes, sha256 {}…)", f.name, f.size, &f.sha256[..12]);
    }
    println!("done: set [captions] model_dir = \"{}\" and model = \"{name}\"", dir.display());
    ExitCode::SUCCESS
}

fn download(url: &str, path: &Path, size: u64, sha256: &str) -> Result<(), String> {
    let tmp = path.with_extension("part");
    let mut resp = ureq::get(url).call().map_err(|e| format!("GET {url}: {e}"))?;
    let mut body = resp.body_mut().with_config().limit(size + 1).reader();
    let mut file = std::fs::File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut got = 0u64;
    loop {
        let n = body.read(&mut buf).map_err(|e| format!("reading {url}: {e}"))?;
        if n == 0 {
            break;
        }
        got += n as u64;
        hash.update(&buf[..n]);
        file.write_all(&buf[..n]).map_err(|e| format!("{}: {e}", tmp.display()))?;
    }
    file.sync_all().map_err(|e| format!("{}: {e}", tmp.display()))?;
    let hex: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if got != size || hex != sha256 {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("got {got} bytes with sha256 {hex}, expected {size} bytes with sha256 {sha256}"));
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(toml_text: &str) -> CaptionsSection {
        let cfg: crate::config::Config = toml::from_str(toml_text).unwrap();
        cfg.captions
    }

    #[test]
    fn off_by_default() {
        assert_eq!(CaptionsSection::default().to_runtime(2000).unwrap(), None);
        assert_eq!(section("[captions]\nmodel = \"small\"\n").to_runtime(2000).unwrap(), None);
    }

    #[test]
    fn stream_rules_become_the_runtime_config() {
        let s = section(
            "[captions]\nmodel_dir = \"/var/lib/caudal/models\"\nmodel = \"small\"\nthreads = 4\ndevice = \"cpu\"\n\n\
             [[captions.stream]]\nstreams = [\"noticias*\"]\nlanguage = \"es\"\n\n\
             [[captions.stream]]\nstreams = [\"*\"]\nlanguage = \"auto\"\n",
        );
        let rt = s.to_runtime(2000).unwrap().unwrap();
        assert_eq!(rt.model_dir, PathBuf::from("/var/lib/caudal/models"));
        assert_eq!((rt.model.as_str(), rt.threads, rt.max_streams, rt.min_display_ms), ("small", 4, 2, 3000));
        assert_eq!(rt.device, caudal_captions::DeviceChoice::Cpu);
        assert_eq!(rt.rules[0].language, caudal_captions::Language::Fixed("es".into()));
        assert_eq!(rt.rules[1].language, caudal_captions::Language::Auto);
    }

    #[test]
    fn bad_values_name_the_key() {
        let base = "[[captions.stream]]\nstreams = [\"a\"]\nlanguage = \"es\"\n";
        for (extra, want) in [
            ("[captions]\nthreads = 0\n", "threads"),
            ("[captions]\nthreads = 65\n", "threads"),
            ("[captions]\ndevice = \"cuda\"\n", "device"),
            ("[captions]\nmodel = \"../etc\"\n", "model"),
            ("[captions]\nmax_streams = 0\n", "max_streams"),
        ] {
            let err = section(&format!("{extra}\n{base}")).to_runtime(2000).unwrap_err();
            assert!(err.contains(want), "{extra}: {err}");
        }
        let err =
            section("[[captions.stream]]\nstreams = [\"a\"]\nlanguage = \"Spanish\"\n").to_runtime(2000).unwrap_err();
        assert!(err.contains("language"), "{err}");
        assert!(
            toml::from_str::<crate::config::Config>("[captions]\nmodle = \"x\"\n").is_err(),
            "unknown keys rejected"
        );
    }
}
