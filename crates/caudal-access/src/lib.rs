//! Per-stream network access control: `[[access]]` rules matched by a
//! stream-name glob, each naming CIDR / `country:XX` entries to allow or
//! deny playing and publishing. Installed into `caudal_core::Registry`'s
//! gate alongside token auth (see `crates/caudal/src/subsystems.rs`'s
//! `CombinedGate`), so every protocol that already calls
//! `Registry::authorize` gets it for free.
//!
//! Rule order matters: the first `[[access]]` rule whose `streams` glob
//! matches the stream name decides both `play` and `publish` for it; later
//! rules are never consulted for that stream. A stream matched by no rule
//! is unrestricted (same "open unless configured" default as `[auth]`).
//! Within a matching rule: a deny entry always wins over an allow entry; an
//! empty allow list is not a whitelist (only the deny list applies); a
//! non-empty allow list *is* a whitelist (an address matching neither list
//! is denied). See `rule::evaluate`.
//!
//! `country:` entries need `[access] geoip_db` (a MaxMind DB file the
//! operator supplies; see `geo`); a `country:` entry with no database
//! configured is a config error, caught by [`AccessConfig::checker`] /
//! [`Checker::reload`] (both call [`AccessConfig::build`]), which is what
//! `caudal check` and a hot reload run.
//!
//! [`Checker`] is created once at startup (even with an empty rule list,
//! same convention as `caudal-transcode`'s always-on ladders) and its
//! ruleset is swapped in place on reload via `arc-swap`, so denial counters
//! (for `/metrics`) survive a reload instead of resetting to zero.

mod geo;
mod rule;

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arc_swap::ArcSwap;
use caudal_core::Access;
use parking_lot::Mutex;

pub use rule::{Entry, Rule};

/// `[access]` plus every `[[access]]` entry, as the runtime type. Built by
/// `crates/caudal/src/config.rs` from the TOML section.
#[derive(Debug, Clone, Default)]
pub struct AccessConfig {
    pub rules: Vec<Rule>,
    /// Path to a MaxMind DB (or DB-IP, same format) file. Required when any
    /// rule has a `country:` entry; never bundled or downloaded.
    pub geoip_db: Option<PathBuf>,
}

impl AccessConfig {
    fn build(&self) -> Result<State, String> {
        let needs_geo = self.rules.iter().any(Rule::needs_geo);
        let geo = match (&self.geoip_db, needs_geo) {
            (Some(path), _) => Some(Arc::new(geo::GeoDb::open(path)?)),
            (None, true) => {
                return Err("[[access]] has a `country:` entry but [access] `geoip_db` is not set".to_owned());
            }
            (None, false) => None,
        };
        Ok(State { rules: self.rules.clone(), geo })
    }

    /// Validates the config (same check `build` does) without keeping the
    /// result; what `Config::validate` / `caudal check` calls.
    pub fn validate(&self) -> Result<(), String> {
        self.build().map(drop)
    }

    /// Builds a fresh, standalone [`Checker`]. Used at startup.
    pub fn checker(&self) -> Result<Arc<Checker>, String> {
        let state = self.build()?;
        Ok(Arc::new(Checker { state: ArcSwap::from_pointee(state), denied: Mutex::new(HashMap::new()) }))
    }
}

struct State {
    rules: Vec<Rule>,
    geo: Option<Arc<geo::GeoDb>>,
}

/// Why a request was refused: a low-cardinality `reason` for the
/// `caudal_access_denied_total{stream,reason}` counter, plus a
/// human-readable `detail` for the log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessDenied {
    pub reason: &'static str,
    pub detail: String,
}

impl std::fmt::Display for AccessDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for AccessDenied {}

/// One row of the `/metrics` counter.
pub struct DeniedMetric {
    pub stream: String,
    pub reason: &'static str,
    pub count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DeniedKey {
    stream: String,
    reason: &'static str,
}

/// Checks `[[access]]` rules for one publish/play request. Cheap to share
/// (an `Arc` internally); [`Checker::reload`] is the only way its ruleset
/// changes after construction.
pub struct Checker {
    state: ArcSwap<State>,
    denied: Mutex<HashMap<DeniedKey, AtomicU64>>,
}

impl Checker {
    /// Swaps in a freshly validated ruleset. Rejects (and leaves the old
    /// ruleset running) an invalid one, same contract as
    /// `Supervisor::reload`'s other sections.
    pub fn reload(&self, cfg: &AccessConfig) -> Result<(), String> {
        let state = cfg.build()?;
        self.state.store(Arc::new(state));
        Ok(())
    }

    /// `Ok(())` when no rule matches `stream`, or the matching rule allows
    /// `access` from `ip`. `ip` is `None` when the protocol has no way to
    /// learn the caller's address (see `caudal_core::Gate`); a rule that
    /// actually restricts this stream then denies with `reason: "no_ip"`
    /// (fail closed) rather than silently letting it through.
    pub fn check(&self, access: Access, stream: &str, ip: Option<IpAddr>) -> Result<(), AccessDenied> {
        let state = self.state.load();
        let Some(rule) = state.rules.iter().find(|r| r.matches_stream(stream)) else { return Ok(()) };
        let (allow, deny) = match access {
            Access::Play => (&rule.play_allow, &rule.play_deny),
            Access::Publish => (&rule.publish_allow, &rule.publish_deny),
        };
        let result = rule::evaluate(allow, deny, ip, state.geo.as_deref());
        if let Err(d) = &result {
            self.bump(stream, d.reason);
        }
        result
    }

    fn bump(&self, stream: &str, reason: &'static str) {
        let mut denied = self.denied.lock();
        denied.entry(DeniedKey { stream: stream.to_owned(), reason }).or_default().fetch_add(1, Ordering::Relaxed);
    }

    /// Every denial counter that has fired at least once. Rendered by
    /// `crates/caudal/src/metrics.rs` as
    /// `caudal_access_denied_total{stream,reason}`.
    pub fn metrics(&self) -> Vec<DeniedMetric> {
        self.denied
            .lock()
            .iter()
            .map(|(k, v)| DeniedMetric { stream: k.stream.clone(), reason: k.reason, count: v.load(Ordering::Relaxed) })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(streams: &[&str]) -> Rule {
        Rule { streams: streams.iter().map(|s| s.to_string()).collect(), ..Default::default() }
    }

    #[test]
    fn no_rules_is_unrestricted() {
        let checker = AccessConfig::default().checker().unwrap();
        assert_eq!(checker.check(Access::Play, "anything", None), Ok(()));
    }

    #[test]
    fn first_matching_rule_wins_and_later_rules_are_ignored() {
        let r1 = Rule { publish_deny: vec![Entry::parse("1.2.3.4").unwrap()], ..rule(&["live-*"]) };
        let r2 = Rule { publish_deny: vec![], ..rule(&["live-main"]) };
        let cfg = AccessConfig { rules: vec![r1, r2], geoip_db: None };
        let checker = cfg.checker().unwrap();
        // r1 matches first (glob), so its (restrictive) deny applies even
        // though r2 (a more specific, permissive rule later) also matches.
        let err = checker.check(Access::Publish, "live-main", Some("1.2.3.4".parse().unwrap())).unwrap_err();
        assert_eq!(err.reason, "ip_denied");
    }

    #[test]
    fn unmatched_stream_falls_through_every_rule_unrestricted() {
        let r = Rule { publish_deny: vec![Entry::parse("1.2.3.4").unwrap()], ..rule(&["live-*"]) };
        let cfg = AccessConfig { rules: vec![r], geoip_db: None };
        let checker = cfg.checker().unwrap();
        assert_eq!(checker.check(Access::Publish, "other", Some("1.2.3.4".parse().unwrap())), Ok(()));
    }

    #[test]
    fn country_rule_without_geoip_db_is_a_config_error() {
        let r = Rule { play_deny: vec![Entry::parse("country:KP").unwrap()], ..rule(&["*"]) };
        let cfg = AccessConfig { rules: vec![r], geoip_db: None };
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("geoip_db"), "{err}");
        assert!(cfg.checker().is_err());
    }

    #[test]
    fn denial_counters_persist_across_reload() {
        let r = Rule { play_deny: vec![Entry::parse("1.2.3.4").unwrap()], ..rule(&["*"]) };
        let cfg = AccessConfig { rules: vec![r.clone()], geoip_db: None };
        let checker = cfg.checker().unwrap();
        let ip = Some("1.2.3.4".parse().unwrap());
        assert!(checker.check(Access::Play, "live", ip).is_err());
        assert!(checker.check(Access::Play, "live", ip).is_err());
        assert_eq!(checker.metrics().iter().find(|m| m.stream == "live").map(|m| m.count), Some(2));

        // A reload that only adds an unrelated rule keeps the counter.
        let other = rule(&["vod-*"]);
        checker.reload(&AccessConfig { rules: vec![r, other], geoip_db: None }).unwrap();
        assert_eq!(checker.metrics().iter().find(|m| m.stream == "live").map(|m| m.count), Some(2));
        assert!(checker.check(Access::Play, "live", ip).is_err());
        assert_eq!(checker.metrics().iter().find(|m| m.stream == "live").map(|m| m.count), Some(3));
    }

    #[test]
    fn reload_rejects_an_invalid_config_and_keeps_the_old_one() {
        let good = rule(&["*"]);
        let cfg = AccessConfig { rules: vec![good], geoip_db: None };
        let checker = cfg.checker().unwrap();
        let bad_rule = Rule { play_deny: vec![Entry::parse("country:KP").unwrap()], ..rule(&["*"]) };
        assert!(checker.reload(&AccessConfig { rules: vec![bad_rule], geoip_db: None }).is_err());
        // Old (unrestricted) ruleset still in effect.
        assert_eq!(checker.check(Access::Play, "anything", None), Ok(()));
    }
}
