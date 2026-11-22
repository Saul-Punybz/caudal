//! A node's side of load balancing: sample the machine's load every
//! interval and send it to the origins (an origin records its own report
//! in its local [`Balancer`] too).

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use caudal_core::Registry;
use url::Url;

use crate::balance::{Balancer, GeoPoint, LoadReport};
use crate::token::Secret;

/// What a [`Sampler`] measures.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sample {
    pub cpu_percent: f64,
    pub ram_percent: f64,
    pub bw_out_bps: u64,
    pub viewers: u64,
    pub streams: Vec<String>,
}

/// Measures the node's load. Called once per report interval.
pub trait Sampler: Send + 'static {
    fn sample(&mut self) -> Sample;
}

impl<F: FnMut() -> Sample + Send + 'static> Sampler for F {
    fn sample(&mut self) -> Sample {
        self()
    }
}

/// The real sampler: whole-machine CPU and memory from the OS
/// (`sysinfo`, which covers Linux, macOS and Windows), viewers and bytes
/// sent from the registry's open sessions, live streams from the registry.
///
/// Bandwidth is the sum, over sessions open at both samples, of the bytes
/// sent between them, plus everything a session opened since the last
/// sample has sent; bytes a session sent in its last interval before
/// closing are missed. MistServer measures the network interface instead,
/// which also counts traffic that is not Caudal's.
pub struct SystemSampler {
    /// `None` when the OS refused (`sysinfo::System::new` failed): CPU and
    /// memory then report 0.
    sys: Option<sysinfo::System>,
    registry: Weak<Registry>,
    last: HashMap<u64, u64>,
    last_at: Instant,
}

impl SystemSampler {
    pub fn new(registry: &Arc<Registry>) -> Self {
        let sys = match sysinfo::System::new() {
            Ok(mut sys) => {
                // CPU use is a difference between two refreshes: take the
                // first now.
                sys.refresh_cpu_usage();
                Some(sys)
            }
            Err(e) => {
                tracing::warn!(error = %e, "cluster balance: cannot read CPU/memory use, reporting 0");
                None
            }
        };
        Self { sys, registry: Arc::downgrade(registry), last: HashMap::new(), last_at: Instant::now() }
    }
}

impl Sampler for SystemSampler {
    fn sample(&mut self) -> Sample {
        let (cpu_percent, ram_percent) = match &mut self.sys {
            Some(sys) => {
                sys.refresh_cpu_usage();
                sys.refresh_memory();
                let total = sys.total_memory();
                let ram = if total == 0 { 0.0 } else { sys.used_memory() as f64 * 100.0 / total as f64 };
                (f64::from(sys.global_cpu_usage()).clamp(0.0, 100.0), ram.clamp(0.0, 100.0))
            }
            None => (0.0, 0.0),
        };
        let Some(registry) = self.registry.upgrade() else {
            return Sample { cpu_percent, ram_percent, ..Sample::default() };
        };
        let now = Instant::now();
        let sessions = registry.sessions().list(None);
        let mut sent = 0u64;
        let mut next = HashMap::with_capacity(sessions.len());
        for s in &sessions {
            let before = self.last.get(&s.id).copied().unwrap_or(0);
            sent += s.bytes_out.saturating_sub(before);
            next.insert(s.id, s.bytes_out);
        }
        let secs = now.duration_since(self.last_at).as_secs_f64();
        self.last = next;
        self.last_at = now;
        let bw_out_bps = if secs > 0.0 { (sent as f64 * 8.0 / secs) as u64 } else { 0 };
        let mut streams: Vec<String> =
            registry.list().iter().filter(|s| !s.is_ended()).map(|s| s.name().to_owned()).collect();
        streams.sort();
        Sample { cpu_percent, ram_percent, bw_out_bps, viewers: sessions.len() as u64, streams }
    }
}

/// What a node says about itself besides its load.
#[derive(Debug, Clone)]
pub struct ReporterConfig {
    pub node_id: String,
    pub secret: Secret,
    /// Origins to send reports to (HTTP base URLs). Empty on an origin
    /// that only records its own.
    pub origins: Vec<Url>,
    pub interval: Duration,
    /// Where viewers sent to this node go; `None`: never chosen.
    pub public_url: Option<String>,
    pub accepts_viewers: bool,
    pub capacity_bps: u64,
    pub location: Option<GeoPoint>,
    pub country: Option<String>,
}

impl ReporterConfig {
    fn report(&self, s: Sample) -> LoadReport {
        LoadReport {
            node_id: self.node_id.clone(),
            url: self.public_url.clone(),
            accepts_viewers: self.accepts_viewers,
            cpu_percent: s.cpu_percent,
            ram_percent: s.ram_percent,
            bw_out_bps: s.bw_out_bps,
            bw_capacity_bps: self.capacity_bps,
            viewers: s.viewers,
            streams: s.streams,
            location: self.location,
            country: self.country.clone(),
        }
    }
}

/// Samples every `cfg.interval` and sends the report to every origin
/// (concurrently, failures logged at debug level), and to `local` when
/// this node is an origin. Runs until the handle is aborted. Must be
/// called inside a tokio runtime.
pub fn spawn_reporter(
    cfg: ReporterConfig,
    local: Option<Arc<Balancer>>,
    mut sampler: impl Sampler,
) -> tokio::task::JoinHandle<()> {
    let http = reqwest::Client::builder()
        .timeout(cfg.interval.clamp(Duration::from_millis(500), Duration::from_secs(3)))
        .build()
        .expect("reqwest client");
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(cfg.interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let report = cfg.report(sampler.sample());
            if let Some(b) = &local {
                b.update(report.clone());
            }
            if cfg.origins.is_empty() {
                continue;
            }
            let token = cfg.secret.mint(&cfg.node_id);
            let mut sends = tokio::task::JoinSet::new();
            for o in &cfg.origins {
                let url = format!("{}/api/v1/cluster/report", o.as_str().trim_end_matches('/'));
                let req = http.post(url).bearer_auth(&token).json(&report);
                let origin = o.to_string();
                sends.spawn(async move {
                    if let Err(e) = req.send().await.and_then(|r| r.error_for_status()) {
                        tracing::debug!(%origin, error = %e, "cluster: load report not delivered");
                    }
                });
            }
            // One slow origin delays the next sample, never the others.
            while sends.join_next().await.is_some() {}
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use caudal_core::BufferConfig;

    #[test]
    fn system_sampler_counts_viewers_bytes_and_streams() {
        let registry = Registry::new();
        let _p = registry.publish("cam", BufferConfig::default()).unwrap();
        let mut s = SystemSampler::new(&registry);
        let first = s.sample();
        assert_eq!((first.viewers, first.bw_out_bps), (0, 0));
        assert_eq!(first.streams, ["cam"]);
        assert!((0.0..=100.0).contains(&first.cpu_percent));
        assert!(first.ram_percent > 0.0 && first.ram_percent <= 100.0, "{}", first.ram_percent);

        let a = registry.open_session("cam", "hls", None);
        a.add_bytes(1_000);
        std::thread::sleep(Duration::from_millis(50));
        let second = s.sample();
        assert_eq!(second.viewers, 1);
        assert!(second.bw_out_bps > 0);
        // Only the bytes sent since the last sample count.
        a.add_bytes(500);
        let b = registry.open_session("cam", "whep", None);
        b.add_bytes(250);
        std::thread::sleep(Duration::from_millis(50));
        let before = s.last_at;
        let third = s.sample();
        let secs = s.last_at.duration_since(before).as_secs_f64();
        assert_eq!(third.viewers, 2);
        let expect = 750.0 * 8.0 / secs;
        assert!((third.bw_out_bps as f64 - expect).abs() <= expect * 0.01 + 1.0, "{} vs {expect}", third.bw_out_bps);
        drop((a, b));
        assert_eq!(s.sample().viewers, 0);
    }
}
