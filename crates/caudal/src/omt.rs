//! `[omt]` at runtime: the one shared discovery (mDNS responder and/or
//! discovery server) plus the source directory browsing it, the running
//! `[[omt.pull]]`s and `[[omt.output]]`s, and what the API and `/metrics`
//! read from them.
//!
//! Discovery starts only when something needs it: a pull by full name or
//! any output at startup or reload, or the first `GET /api/v1/omt/sources`.
//! A server without `[omt]` never opens the mDNS socket. If discovery fails
//! to start (no multicast interface, a bad server host), pulls by `omt://`
//! URL still work and the next use tries again.

use std::sync::Arc;

use caudal_core::Registry;
use open_media_transport::address::Directory;
use open_media_transport::command::Quality;
use open_media_transport::discovery::{Discovery, DiscoveryConfig, Source};
use parking_lot::Mutex;

use crate::config::Config;

/// The shared discovery and the directory browsing it.
#[derive(Clone)]
struct Shared {
    discovery: Option<Arc<Discovery>>,
    directory: Arc<Directory>,
}

pub struct OmtRuntime {
    /// Used while discovery has not started; fixed once it has.
    discovery_config: Mutex<DiscoveryConfig>,
    shared: Mutex<Option<Shared>>,
    pulls: caudal_omt::PullHandle,
    outputs: caudal_omt::OutputHandle,
}

/// What a reload did to `[omt]`, in `ReloadReport` terms.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct OmtReload {
    pub applied: Vec<&'static str>,
    pub requires_restart: Vec<&'static str>,
}

impl OmtRuntime {
    /// Starts every pull and output in `cfg`, and discovery if they need it.
    pub fn start(cfg: &Config, registry: Arc<Registry>, buffer: caudal_core::BufferConfig) -> Arc<Self> {
        let rt = Self::empty(cfg.omt.discovery_config(), registry.clone(), buffer);
        let shared = if cfg.omt.needs_discovery() { rt.shared() } else { None };
        if !cfg.omt.pull.is_empty() {
            rt.pulls.reload(cfg.omt.pull_configs(&cfg.transcode, shared.as_ref().map(|s| s.directory.clone())));
            tracing::info!(pulls = cfg.omt.pull.len(), "omt pulls started");
        }
        if !cfg.omt.output.is_empty() {
            rt.outputs.reload(cfg.omt.output_configs(shared.and_then(|s| s.discovery)));
            tracing::info!(outputs = cfg.omt.output.len(), "omt outputs started");
        }
        Arc::new(rt)
    }

    fn empty(discovery_config: DiscoveryConfig, registry: Arc<Registry>, buffer: caudal_core::BufferConfig) -> Self {
        OmtRuntime {
            discovery_config: Mutex::new(discovery_config),
            shared: Mutex::new(None),
            pulls: caudal_omt::start_pulls(registry.clone(), buffer, Vec::new()),
            outputs: caudal_omt::start_outputs(registry, Vec::new()),
        }
    }

    /// A runtime whose directory is `directory` (fed by hand) and that never
    /// starts discovery: for tests.
    #[cfg(test)]
    pub fn with_directory(directory: Directory, registry: Arc<Registry>) -> Arc<Self> {
        let rt = Self::empty(DiscoveryConfig::default(), registry, caudal_core::BufferConfig::default());
        *rt.shared.lock() = Some(Shared { discovery: None, directory: Arc::new(directory) });
        Arc::new(rt)
    }

    /// The shared discovery, started on first use. `None` (logged) when it
    /// cannot start; the next call tries again. Blocking (starts threads,
    /// may resolve the discovery server's host): call from a blocking
    /// context in async code.
    fn shared(&self) -> Option<Shared> {
        let mut shared = self.shared.lock();
        if let Some(s) = shared.as_ref() {
            return Some(s.clone());
        }
        let config = self.discovery_config.lock().clone();
        let started = Discovery::with_config(&config).map(Arc::new).and_then(|d| {
            let directory = Directory::with_shared(d.clone())?;
            Ok(Shared { discovery: Some(d), directory: Arc::new(directory) })
        });
        match started {
            Ok(s) => {
                tracing::info!(
                    server = config.server.as_deref().unwrap_or("-"),
                    interfaces = ?config.interfaces.only,
                    "omt discovery started"
                );
                *shared = Some(s.clone());
                Some(s)
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "omt discovery failed to start: pulls by name and output announcements will not work (omt:// URLs still do)"
                );
                None
            }
        }
    }

    /// Applies `[omt]` from `new` (already validated). Pull and output lists
    /// reload live, keeping unchanged entries running; the discovery
    /// settings are fixed once discovery has started.
    pub fn reload(&self, old: &Config, new: &Config) -> OmtReload {
        let mut report = OmtReload::default();
        if old.omt.discovery_key() != new.omt.discovery_key() {
            if self.shared.lock().is_some() {
                report.requires_restart.push("omt.discovery");
            } else {
                *self.discovery_config.lock() = new.omt.discovery_config();
                report.applied.push("omt.discovery");
            }
        }
        let pulls_changed =
            old.omt.pull != new.omt.pull || old.omt.ffmpeg(&old.transcode) != new.omt.ffmpeg(&new.transcode);
        let outputs_changed = old.omt.output != new.omt.output;
        if !pulls_changed && !outputs_changed {
            return report;
        }
        let shared = if new.omt.needs_discovery() { self.shared() } else { self.shared.lock().clone() };
        if pulls_changed {
            self.pulls.reload(new.omt.pull_configs(&new.transcode, shared.as_ref().map(|s| s.directory.clone())));
            report.applied.push("omt.pull");
        }
        if outputs_changed {
            self.outputs.reload(new.omt.output_configs(shared.and_then(|s| s.discovery)));
            report.applied.push("omt.output");
        }
        report
    }

    /// Every source discovered now, sorted by full name, and whether the
    /// directory was started by this call (it has had no time to hear
    /// anything yet). Blocking, see [`OmtRuntime::shared`].
    pub fn sources(&self) -> Result<(Vec<Source>, bool), String> {
        let fresh = self.shared.lock().is_none();
        let shared = self.shared().ok_or_else(|| "OMT discovery could not start; see the server log".to_string())?;
        Ok((shared.directory.sources(), fresh))
    }

    pub fn pulls(&self) -> Vec<caudal_omt::PullStatus> {
        self.pulls.status()
    }

    pub fn outputs(&self) -> Vec<caudal_omt::OutputStatus> {
        self.outputs.status()
    }

    /// For tests that need a running pull/output entry without a sender.
    #[cfg(test)]
    pub fn reload_for_test(&self, pulls: Vec<caudal_omt::PullConfig>, outputs: Vec<caudal_omt::OutputConfig>) {
        self.pulls.reload(pulls);
        self.outputs.reload(outputs);
    }
}

pub fn quality_str(q: Quality) -> &'static str {
    match q {
        Quality::Default => "default",
        Quality::Low => "low",
        Quality::Medium => "medium",
        Quality::High => "high",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(t: &str) -> Config {
        let c: Config = toml::from_str(t).unwrap();
        c.validate().unwrap();
        c
    }

    /// Reloads that need no discovery (pulls by URL only), so no mDNS socket
    /// opens in the test.
    #[tokio::test]
    async fn reload_applies_pull_changes_and_discovery_settings_before_start() {
        let registry = Registry::new();
        let a = cfg("[[omt.pull]]\nstream = \"cam\"\nurl = \"omt://127.0.0.1:6400\"\n");
        let rt = OmtRuntime::start(&a, registry, caudal_core::BufferConfig::default());
        assert_eq!(rt.pulls().len(), 1);
        assert!(rt.shared.lock().is_none(), "no discovery for URL pulls");

        assert_eq!(rt.reload(&a, &a), OmtReload::default(), "nothing changed");

        let b = cfg("[omt]\ninterfaces = [\"en0\"]\n[[omt.pull]]\nstream = \"cam\"\nurl = \"omt://127.0.0.1:6401\"\n\
                     [[omt.pull]]\nstream = \"cam2\"\nurl = \"omt://127.0.0.1:6402\"\n");
        let r = rt.reload(&a, &b);
        assert_eq!(r.applied, vec!["omt.discovery", "omt.pull"]);
        assert!(r.requires_restart.is_empty(), "discovery not started yet: its settings apply live");
        assert_eq!(rt.discovery_config.lock().interfaces.only, vec!["en0".to_string()]);
        let pulls = rt.pulls();
        assert_eq!(
            pulls.iter().map(|p| p.source.as_str()).collect::<Vec<_>>(),
            ["omt://127.0.0.1:6401", "omt://127.0.0.1:6402"]
        );

        // [transcode] ffmpeg changes what the pulls run when [omt] has none.
        let c = cfg(&format!(
            "[transcode]\nffmpeg = \"/x/ffmpeg\"\n{}",
            "[omt]\ninterfaces = [\"en0\"]\n[[omt.pull]]\nstream = \"cam\"\nurl = \"omt://127.0.0.1:6401\"\n\
             [[omt.pull]]\nstream = \"cam2\"\nurl = \"omt://127.0.0.1:6402\"\n"
        ));
        assert_eq!(rt.reload(&b, &c).applied, vec!["omt.pull"]);
    }

    #[tokio::test]
    async fn discovery_settings_need_a_restart_once_started() {
        let rt = OmtRuntime::with_directory(Directory::manual(), Registry::new());
        let a = cfg("");
        let b = cfg("[omt]\ndiscovery_server = \"omt://10.0.0.2\"\n");
        let r = rt.reload(&a, &b);
        assert_eq!(r.requires_restart, vec!["omt.discovery"]);
        assert!(r.applied.is_empty());
    }
}
