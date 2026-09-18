//! TOML configuration: every section and key optional with fixed defaults,
//! unknown keys rejected so a typo fails loudly instead of silently no-op-ing.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use caudal_core::BufferConfig;
use serde::Deserialize;

fn default_http_bind() -> SocketAddr {
    "0.0.0.0:8080".parse().unwrap()
}

fn default_rtmp_bind() -> SocketAddr {
    "0.0.0.0:1935".parse().unwrap()
}

fn default_rtmp_app() -> String {
    "live".to_string()
}

fn default_srt_bind() -> SocketAddr {
    "0.0.0.0:9000".parse().unwrap()
}

fn default_srt_latency_ms() -> u32 {
    120
}

fn default_part_ms() -> u32 {
    200
}

fn default_segment_ms() -> u32 {
    2000
}

fn default_window_secs() -> u64 {
    50
}

fn default_max_mb() -> usize {
    256
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct ServerSection {
    #[serde(default = "default_http_bind")]
    pub http_bind: SocketAddr,
}

impl Default for ServerSection {
    fn default() -> Self {
        Self { http_bind: default_http_bind() }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct RtmpSection {
    #[serde(default = "default_rtmp_bind")]
    pub bind: SocketAddr,
    #[serde(default = "default_rtmp_app")]
    pub app: String,
}

impl Default for RtmpSection {
    fn default() -> Self {
        Self { bind: default_rtmp_bind(), app: default_rtmp_app() }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct SrtSection {
    #[serde(default = "default_srt_bind")]
    pub bind: SocketAddr,
    #[serde(default = "default_srt_latency_ms")]
    pub latency_ms: u32,
    /// AES passphrase callers must use; none means unencrypted.
    pub passphrase: Option<String>,
}

impl Default for SrtSection {
    fn default() -> Self {
        Self { bind: default_srt_bind(), latency_ms: default_srt_latency_ms(), passphrase: None }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct HlsSection {
    #[serde(default = "default_part_ms")]
    pub part_ms: u32,
    #[serde(default = "default_segment_ms")]
    pub segment_ms: u32,
}

impl Default for HlsSection {
    fn default() -> Self {
        Self { part_ms: default_part_ms(), segment_ms: default_segment_ms() }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct BufferSection {
    #[serde(default = "default_window_secs")]
    pub window_secs: u64,
    #[serde(default = "default_max_mb")]
    pub max_mb: usize,
}

impl Default for BufferSection {
    fn default() -> Self {
        Self { window_secs: default_window_secs(), max_mb: default_max_mb() }
    }
}

impl BufferSection {
    pub fn to_buffer_config(self) -> BufferConfig {
        BufferConfig { window: Duration::from_secs(self.window_secs), max_bytes: self.max_mb * 1024 * 1024 }
    }
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub server: ServerSection,
    pub rtmp: RtmpSection,
    pub srt: SrtSection,
    pub hls: HlsSection,
    pub buffer: BufferSection,
}

/// Loads and validates a config file, rejecting unknown keys. The error
/// message names the offending key and its line number (from `toml`'s
/// span-aware `Display` impl).
pub fn load(path: &Path) -> Result<Config, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    toml::from_str(&text).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_empty() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.server.http_bind, default_http_bind());
        assert_eq!(cfg.rtmp.bind, default_rtmp_bind());
        assert_eq!(cfg.rtmp.app, "live");
        assert_eq!(cfg.hls.part_ms, 200);
        assert_eq!(cfg.hls.segment_ms, 2000);
        assert_eq!(cfg.buffer.window_secs, 50);
        assert_eq!(cfg.buffer.max_mb, 256);
    }

    #[test]
    fn partial_section_keeps_other_defaults() {
        let cfg: Config = toml::from_str("[hls]\npart_ms = 100\n").unwrap();
        assert_eq!(cfg.hls.part_ms, 100);
        assert_eq!(cfg.hls.segment_ms, 2000);
    }

    #[test]
    fn unknown_key_reports_name_and_line() {
        let src = "[server]\nhttp_bind = \"127.0.0.1:0\"\nhttp_bnid = 1\n";
        let err = toml::from_str::<Config>(src).unwrap_err().to_string();
        assert!(err.contains("http_bnid"), "{err}");
        assert!(err.contains("line 3"), "{err}");
    }

    #[test]
    fn buffer_section_converts_to_core_config() {
        let cfg: Config = toml::from_str("[buffer]\nwindow_secs = 10\nmax_mb = 1\n").unwrap();
        let bc = cfg.buffer.to_buffer_config();
        assert_eq!(bc.window, Duration::from_secs(10));
        assert_eq!(bc.max_bytes, 1024 * 1024);
    }

    #[test]
    fn load_missing_file_is_an_error_not_a_panic() {
        let err = load(Path::new("/nonexistent/caudal.toml")).unwrap_err();
        assert!(err.contains("nonexistent"), "{err}");
    }
}
