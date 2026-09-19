//! Starts every ingest/output subsystem from a [`crate::config::Config`]
//! and, in [`Supervisor`], keeps what a config reload needs to apply the
//! smallest correct change per section:
//!
//! - **Hot, no restart:** `[[restream]]`, `[[multicast]]`, `[[channel]]`,
//!   `[[failover]]`, `[[srt.push]]`, `[[rtsp.pull]]` are each their own subsystem with a `reload` that
//!   diffs by identity — unchanged entries keep their task untouched, so
//!   a reload never drops an unrelated viewer or publisher. `[auth]` and
//!   `[hooks]` swap in place (`Registry::set_gate` is a live snapshot
//!   swap; a changed webhook config restarts only its own forwarder
//!   task). `[access]` / `[[access.rules]]` swap in place too
//!   (`caudal_access::Checker::reload`, an `arc-swap` internally, see its
//!   docs) without ever touching `Registry::set_gate`. Neither `[auth]`
//!   nor `[[access.rules]]` re-checks a session already granted: like
//!   `[transcode]`'s ladders (read fresh, via `arc-swap`, by every *new*
//!   publish; a transcode already running keeps the snapshot it started
//!   with), a new ruleset governs only the *next* publish/play request —
//!   an existing viewer or publisher a new deny rule would now refuse
//!   keeps streaming until it disconnects on its own. Revoking an
//!   in-progress session needs killing its connection some other way
//!   (there is no per-session kill switch today).
//! - **Restarted (this listener only):** `[rtmp]`, `[srt]`'s bind/
//!   latency/passphrase, and `[rtsp]`'s server/RTSPS fields each restart
//!   only their own listener task. Aborting it never drops an
//!   already-accepted connection: every connection handler is its own
//!   independently spawned task, not a child of the listener's accept
//!   loop.
//! - **`requires_restart`:** `[server]`, `[tls]`, `[webrtc]`, `[moq]`,
//!   `[hls]`, `[record]`, `[buffer]`, `[admin]`, `[health]`, `[cluster]` are wired once at startup into the
//!   single `axum::Router` passed to `axum::serve`; swapping them without
//!   restarting the process is out of scope here. A reload reports these
//!   sections changed, never silently ignoring them.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use caudal_core::Registry;
use parking_lot::Mutex;

use crate::config::{self, Config};

/// Plugs `caudal-auth` (token) and `caudal-access` (IP/CIDR/country) into
/// the core's access gate as one combined check: access rules first (they
/// short-circuit on a network-level denial without even looking at the
/// token), then token auth. Either half can be a no-op (no `[auth]` keys,
/// no `[[access]]` rules) without disabling the other.
pub struct CombinedGate {
    auth: caudal_auth::Authorizer,
    /// Always present (even with an empty ruleset) once the registry has a
    /// gate installed at all; see `Supervisor::start`. Its own ruleset is
    /// swapped by `caudal_access::Checker::reload`, independent of `auth`.
    access: Arc<caudal_access::Checker>,
    /// `[cluster]`'s secret: a valid inter-node token may play any stream,
    /// before `[[access.rules]]` and `[auth]` (an edge enforces those for
    /// its own viewers). `None` outside a cluster.
    cluster: Option<caudal_cluster::Secret>,
}

impl caudal_core::Gate for CombinedGate {
    fn check<'a>(
        &'a self,
        access: caudal_core::Access,
        stream: &'a str,
        token: Option<&'a str>,
        ip: Option<std::net::IpAddr>,
    ) -> caudal_core::GateFuture<'a> {
        Box::pin(async move {
            if access == caudal_core::Access::Play
                && let (Some(secret), Some(t)) = (&self.cluster, token)
                && secret.verify(t).is_some()
            {
                return Ok(());
            }
            if let Err(denied) = self.access.check(access, stream, ip) {
                tracing::info!(
                    stream, ip = ?ip, reason = denied.reason, detail = %denied.detail,
                    action = ?access, "access denied",
                );
                return Err(caudal_core::Denied::Refused(denied.to_string()));
            }
            let action = match access {
                caudal_core::Access::Publish => caudal_auth::Action::Publish,
                caudal_core::Access::Play => caudal_auth::Action::Play,
            };
            self.auth.check(action, stream, token).await.map_err(|e| match e {
                caudal_auth::AuthError::Missing => caudal_core::Denied::Missing,
                other => caudal_core::Denied::Refused(other.to_string()),
            })
        })
    }
}

/// Applies the loaded `[auth]` section to the registry: reinstalls the
/// combined gate with a fresh `Authorizer`, keeping the current
/// `caudal-access` checker (its ruleset is independent, see
/// `apply_access`).
fn apply_auth(
    registry: &Arc<Registry>,
    access: &Arc<caudal_access::Checker>,
    cluster: &Option<caudal_cluster::Secret>,
    section: &config::AuthSection,
) {
    let auth = section.to_auth_config().expect("validated");
    if auth.keys.is_none() {
        tracing::warn!("no [auth] keys: anyone who can reach the server can publish and play (subject to [[access]])");
    } else {
        tracing::info!(publish = auth.publish, play = auth.play, "token auth enabled");
    }
    registry.set_gate(Arc::new(CombinedGate {
        auth: caudal_auth::Authorizer::new(auth),
        access: access.clone(),
        cluster: cluster.clone(),
    }));
}

/// Applies the loaded `[access]` / `[[access]]` sections: swaps the
/// ruleset in place on the existing `caudal-access::Checker` (see its
/// module docs), so denial counters and the installed gate object survive.
/// `Err` only on a config accepted by `serde` but rejected by
/// `AccessConfig::build` (e.g. a `country:` entry with a `geoip_db` that
/// stopped opening); `Config::validate` already runs the same check before
/// a reload gets here, so this is a safety net, not the primary guard.
fn apply_access(access: &Arc<caudal_access::Checker>, section: &config::AccessSection) -> Result<(), String> {
    access.reload(&section.to_access_config().expect("validated"))
}

/// Sends a webhook for every stream that starts or ends, until aborted.
fn spawn_hooks(registry: &Arc<Registry>, hooks: caudal_auth::Hooks) -> tokio::task::JoinHandle<()> {
    let mut started = registry.subscribe_publishes();
    let mut ended = registry.subscribe_ends();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                s = started.recv() => match s {
                    Ok(s) => hooks.emit(caudal_auth::HookEvent::StreamStarted { stream: s.name().to_owned() }),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => tracing::warn!(missed = n, "webhooks lagged"),
                    Err(_) => return,
                },
                e = ended.recv() => match e {
                    Ok(name) => hooks.emit(caudal_auth::HookEvent::StreamEnded { stream: name.to_string() }),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => tracing::warn!(missed = n, "webhooks lagged"),
                    Err(_) => return,
                },
            }
        }
    })
}

/// Applies `[hooks]`: aborts the previous forwarder (if any) and starts a
/// new one when webhooks are configured.
fn apply_hooks(
    registry: &Arc<Registry>,
    task: &Mutex<Option<tokio::task::JoinHandle<()>>>,
    section: &config::HooksSection,
) {
    if let Some(old) = task.lock().take() {
        old.abort();
    }
    if let Some(hooks) = section.to_hooks_config().expect("validated") {
        *task.lock() = Some(spawn_hooks(registry, caudal_auth::Hooks::new(Some(hooks))));
    }
}

/// The RTMP listener task, restarted whole when `[rtmp]` changes. The
/// compare key is simply `old.rtmp != new.rtmp` on the last-applied
/// `Config` (see `Supervisor::reload`), so there's nothing to keep here
/// beyond the task itself.
struct RtmpListener {
    task: tokio::task::JoinHandle<()>,
}

fn spawn_rtmp(
    registry: Arc<Registry>,
    buffer: caudal_core::BufferConfig,
    section: &config::RtmpSection,
) -> RtmpListener {
    let cfg = caudal_rtmp::RtmpConfig { bind: section.bind, app: section.app.clone(), buffer };
    tracing::info!(bind = %cfg.bind, app = %cfg.app, "starting rtmp listener");
    let task = tokio::spawn(async move {
        match caudal_rtmp::serve(cfg, registry).await {
            Ok(()) => tracing::info!("rtmp listener stopped"),
            Err(e) => tracing::error!(error = %e, "rtmp listener failed"),
        }
    });
    RtmpListener { task }
}

/// The `[srt]` core (bind, latency, passphrase) that decides whether the
/// SRT *listener* needs restarting; `[[srt.push]]` is a separate subsystem
/// (`caudal_srt::PushHandle`) and never part of this key.
type SrtCoreKey = (SocketAddr, u32, Option<String>);

fn srt_core_key(s: &config::SrtSection) -> SrtCoreKey {
    (s.bind, s.latency_ms, s.passphrase.clone())
}

struct SrtListener {
    key: SrtCoreKey,
    task: tokio::task::JoinHandle<()>,
}

fn spawn_srt(registry: Arc<Registry>, buffer: caudal_core::BufferConfig, section: &config::SrtSection) -> SrtListener {
    let cfg = caudal_srt::SrtConfig {
        bind: section.bind,
        latency_ms: section.latency_ms,
        passphrase: section.passphrase.clone(),
        buffer,
    };
    tracing::info!(bind = %cfg.bind, "starting srt listener");
    let task = tokio::spawn(async move {
        match caudal_srt::serve(cfg, registry).await {
            Ok(()) => tracing::info!("srt listener stopped"),
            Err(e) => tracing::error!(error = %e, "srt listener failed"),
        }
    });
    SrtListener { key: srt_core_key(section), task }
}

/// The `[rtsp]` core (server bind, RTSPS, UDP range) that decides whether
/// the RTSP *server* task needs restarting; `[[rtsp.pull]]` is a separate
/// subsystem (`caudal_rtsp::PullHandle`) and never part of this key.
type RtspCoreKey = (Option<SocketAddr>, (u16, u16), Option<SocketAddr>, Option<PathBuf>, Option<PathBuf>);

fn rtsp_core_key(s: &config::RtspSection) -> RtspCoreKey {
    (s.bind, s.udp_port_range, s.tls_bind, s.tls_cert.clone(), s.tls_key.clone())
}

struct RtspListener {
    key: RtspCoreKey,
    /// `None` when `[rtsp].bind` is unset: nothing to serve.
    task: Option<tokio::task::JoinHandle<()>>,
}

fn spawn_rtsp(
    registry: Arc<Registry>,
    buffer: caudal_core::BufferConfig,
    section: &config::RtspSection,
) -> RtspListener {
    let key = rtsp_core_key(section);
    let task = section.bind.map(|bind| {
        let cfg = caudal_rtsp::RtspConfig {
            bind: Some(bind),
            buffer,
            tls: section.to_tls_config().expect("validated"),
            udp_port_range: section.udp_port_range,
            session_timeout: caudal_rtsp::DEFAULT_SESSION_TIMEOUT,
        };
        tracing::info!(%bind, "starting rtsp server");
        tokio::spawn(async move {
            match caudal_rtsp::serve(cfg, registry).await {
                Ok(()) => tracing::info!("rtsp server stopped"),
                Err(e) => tracing::error!(error = %e, "rtsp server failed"),
            }
        })
    });
    RtspListener { key, task }
}

/// What a reload changed. Serialized as the `POST /api/v1/config/reload`
/// response body.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct ReloadReport {
    /// Sections applied live, no listener touched.
    pub applied: Vec<String>,
    /// Listeners restarted (bind or another core field changed); their own
    /// connections are unaffected, only new ones briefly wait to connect.
    pub restarted: Vec<String>,
    /// Sections that changed but need a full process restart to take
    /// effect, e.g. `[server]`'s `http_bind`. Never silently ignored.
    pub requires_restart: Vec<String>,
}

/// Every running subsystem plus the bookkeeping a reload needs. Built once
/// by [`Supervisor::start`]; `crate::main::run` keeps it alive for the
/// process's lifetime and wires `POST /api/v1/config/reload` / SIGHUP to
/// [`Supervisor::reload`].
pub struct Supervisor {
    registry: Arc<Registry>,
    /// `[buffer]` is not hot (see module docs); captured once so a reload
    /// spawning a new restream/channel/pull/push/transcode task uses the
    /// same buffer sizing as everything already running.
    buffer: caudal_core::BufferConfig,
    /// `[server] trusted_proxies`, parsed once at startup. `[server]` is a
    /// `requires_restart` section (see module docs), so this never changes
    /// without a full restart either, even though `caudal-channel`'s
    /// `reload` runs independently of it.
    trusted_proxies: Vec<caudal_core::Cidr>,

    rtmp: Mutex<RtmpListener>,
    srt_listen: Mutex<SrtListener>,
    rtsp_listen: Mutex<RtspListener>,

    restream: caudal_restream::RestreamHandle,
    multicast: caudal_multicast::MulticastHandle,
    channel: caudal_channel::ChannelHandle,
    failover: caudal_failover::FailoverHandle,
    srt_push: caudal_srt::PushHandle,
    rtsp_pull: caudal_rtsp::PullHandle,
    transcode: caudal_transcode::TranscodeHandle,
    hooks_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// `[[access]]` / `[access]`. Created once (even empty); reloaded in
    /// place (see `apply_access`), never replaced, so `/metrics`' denial
    /// counters survive a reload.
    access: Arc<caudal_access::Checker>,
    /// `[cluster]`'s secret, fixed at startup (`[cluster]` needs a restart).
    cluster: Option<caudal_cluster::Secret>,

    /// The last config a reload was applied against (or the startup
    /// config); also the mutex that serializes concurrent reloads (SIGHUP
    /// racing the API).
    current: Mutex<Config>,
}

/// What `main::run` gets back from starting every subsystem: the
/// supervisor (for reload) plus the routers it must merge into the app,
/// which it doesn't otherwise need to know the shape of.
pub struct Started {
    pub supervisor: Arc<Supervisor>,
    pub restream_router: axum::Router,
    pub multicast_router: axum::Router,
    /// For `/metrics` (`caudal_multicast_*`).
    pub multicast: caudal_multicast::MulticastHandle,
    pub channel_router: axum::Router,
    pub failover_router: axum::Router,
    /// For forwarding switch events to health webhooks.
    pub failover: caudal_failover::FailoverHandle,
    /// For `main::run` to hand to `api::AppState::set_access`, so
    /// `/metrics` can render `caudal_access_denied_total`.
    pub access: Arc<caudal_access::Checker>,
}

impl Supervisor {
    /// Starts every reloadable subsystem (RTMP, SRT, RTSP, restream,
    /// channels, failover, transcode, auth, hooks) from `cfg`. WebRTC, MoQ, HLS,
    /// recording, TLS and the HTTP listener are started by `main::run`
    /// itself: they're wired once into the app `Router` and are not part
    /// of the hot-reload surface (see module docs).
    pub fn start(cfg: &Config, registry: Arc<Registry>) -> Started {
        let buffer = cfg.buffer.to_buffer_config();
        let trusted_proxies = cfg.server.trusted_proxy_cidrs().expect("validated");

        let access = cfg.access.to_access_config().expect("validated").checker().expect("validated");
        let cluster = cfg.cluster.as_ref().map(|c| c.secret().expect("validated"));
        apply_auth(&registry, &access, &cluster, &cfg.auth);
        let hooks_task = Mutex::new(None);
        apply_hooks(&registry, &hooks_task, &cfg.hooks);

        let rtmp = Mutex::new(spawn_rtmp(registry.clone(), buffer, &cfg.rtmp));
        let srt_listen = Mutex::new(spawn_srt(registry.clone(), buffer, &cfg.srt));
        let srt_push = caudal_srt::start_pushes(registry.clone(), cfg.srt.push_targets());
        let rtsp_listen = Mutex::new(spawn_rtsp(registry.clone(), buffer, &cfg.rtsp));
        let rtsp_pull = caudal_rtsp::start_pulls(registry.clone(), buffer, cfg.rtsp.pull_targets());

        let transcode = caudal_transcode::start(registry.clone(), cfg.transcode_runtime_config(buffer))
            .expect("validated transcode config");

        let restream = caudal_restream::start(
            registry.clone(),
            caudal_restream::RestreamConfig { targets: cfg.restream_targets() },
        );
        let restream_router = caudal_restream::router(restream.clone());

        let multicast = caudal_multicast::start(registry.clone(), cfg.multicast_targets().expect("validated"));
        let multicast_router = caudal_multicast::router(multicast.clone());

        let channel = caudal_channel::start(
            registry.clone(),
            caudal_channel::ChannelConfig {
                channels: cfg.channels(),
                buffer,
                trusted_proxies: trusted_proxies.clone(),
            },
        );
        let channel_router = caudal_channel::router(channel.clone());

        let failover = caudal_failover::start(
            registry.clone(),
            caudal_failover::FailoverConfig {
                entries: cfg.failovers().expect("validated"),
                buffer,
                trusted_proxies: trusted_proxies.clone(),
            },
        );
        let failover_router = caudal_failover::router(failover.clone());

        let supervisor = Arc::new(Supervisor {
            registry,
            buffer,
            trusted_proxies,
            rtmp,
            srt_listen,
            rtsp_listen,
            restream,
            multicast: multicast.clone(),
            channel,
            failover: failover.clone(),
            srt_push,
            rtsp_pull,
            transcode,
            hooks_task,
            access: access.clone(),
            cluster,
            current: Mutex::new(cfg.clone()),
        });
        Started {
            supervisor,
            restream_router,
            multicast_router,
            multicast,
            channel_router,
            failover_router,
            failover,
            access,
        }
    }

    /// Validates `new_cfg`, then applies the smallest correct action per
    /// changed section. On an invalid config, nothing changes and the
    /// error is returned (the caller decides how to report it: 400 for
    /// the API, a log line for SIGHUP).
    pub fn reload(&self, new_cfg: Config) -> Result<ReloadReport, String> {
        new_cfg.validate()?;

        // Serializes concurrent reloads (SIGHUP racing the API): the whole
        // apply happens while `current` is locked.
        let mut current = self.current.lock();
        let old = current.clone();
        let mut report = ReloadReport::default();

        if old.restream != new_cfg.restream {
            self.restream.reload(&self.registry, new_cfg.restream_targets());
            report.applied.push("restream".into());
        }
        if old.multicast != new_cfg.multicast {
            self.multicast.reload(&self.registry, new_cfg.multicast_targets().expect("validated"));
            report.applied.push("multicast".into());
        }
        if old.channel != new_cfg.channel {
            self.channel.reload(caudal_channel::ChannelConfig {
                channels: new_cfg.channels(),
                buffer: self.buffer,
                trusted_proxies: self.trusted_proxies.clone(),
            });
            report.applied.push("channel".into());
        }
        if old.failover != new_cfg.failover {
            self.failover.reload(caudal_failover::FailoverConfig {
                entries: new_cfg.failovers().expect("validated"),
                buffer: self.buffer,
                trusted_proxies: self.trusted_proxies.clone(),
            });
            report.applied.push("failover".into());
        }
        if old.srt.push != new_cfg.srt.push {
            self.srt_push.reload(&self.registry, new_cfg.srt.push_targets());
            report.applied.push("srt.push".into());
        }
        if old.rtsp.pull != new_cfg.rtsp.pull {
            self.rtsp_pull.reload(&self.registry, self.buffer, new_cfg.rtsp.pull_targets());
            report.applied.push("rtsp.pull".into());
        }
        if old.transcode != new_cfg.transcode {
            match self.transcode.reload(new_cfg.transcode_runtime_config(self.buffer)) {
                Ok(()) => report.applied.push("transcode".into()),
                // Unreachable in practice: `new_cfg.validate()` above
                // already ran the same check. Kept as a safety net, never
                // silently dropped.
                Err(e) => {
                    tracing::error!(error = %e, "transcode reload rejected after passing validation");
                    report.requires_restart.push("transcode".into());
                }
            }
        }
        if old.auth != new_cfg.auth {
            apply_auth(&self.registry, &self.access, &self.cluster, &new_cfg.auth);
            report.applied.push("auth".into());
        }
        if old.access != new_cfg.access {
            match apply_access(&self.access, &new_cfg.access) {
                Ok(()) => report.applied.push("access".into()),
                // Unreachable in practice: `new_cfg.validate()` above
                // already ran the same check. Kept as a safety net (same
                // pattern as `transcode`'s reload arm above), never
                // silently dropped.
                Err(e) => {
                    tracing::error!(error = %e, "access reload rejected after passing validation");
                    report.requires_restart.push("access".into());
                }
            }
        }
        if old.hooks != new_cfg.hooks {
            apply_hooks(&self.registry, &self.hooks_task, &new_cfg.hooks);
            report.applied.push("hooks".into());
        }

        if old.rtmp != new_cfg.rtmp {
            tracing::info!(
                old_bind = %old.rtmp.bind, new_bind = %new_cfg.rtmp.bind,
                old_app = %old.rtmp.app, new_app = %new_cfg.rtmp.app,
                "[rtmp] changed: restarting the rtmp listener",
            );
            let mut l = self.rtmp.lock();
            l.task.abort();
            *l = spawn_rtmp(self.registry.clone(), self.buffer, &new_cfg.rtmp);
            report.restarted.push("rtmp".into());
        }
        let new_srt_core = srt_core_key(&new_cfg.srt);
        if self.srt_listen.lock().key != new_srt_core {
            tracing::info!(
                old_bind = %old.srt.bind, new_bind = %new_cfg.srt.bind,
                "[srt] changed: restarting the srt listener",
            );
            let mut l = self.srt_listen.lock();
            l.task.abort();
            *l = spawn_srt(self.registry.clone(), self.buffer, &new_cfg.srt);
            report.restarted.push("srt".into());
        }
        let new_rtsp_core = rtsp_core_key(&new_cfg.rtsp);
        if self.rtsp_listen.lock().key != new_rtsp_core {
            tracing::info!(
                old_bind = ?old.rtsp.bind, new_bind = ?new_cfg.rtsp.bind,
                "[rtsp] changed: restarting the rtsp server",
            );
            let mut l = self.rtsp_listen.lock();
            if let Some(task) = l.task.take() {
                task.abort();
            }
            *l = spawn_rtsp(self.registry.clone(), self.buffer, &new_cfg.rtsp);
            report.restarted.push("rtsp".into());
        }

        // Wired once into the app `Router` at startup: applying these
        // without a full process restart is out of scope (module docs).
        // Reported, never silently ignored.
        if old.server != new_cfg.server {
            report.requires_restart.push("server".into());
        }
        if old.tls != new_cfg.tls {
            report.requires_restart.push("tls".into());
        }
        if old.webrtc != new_cfg.webrtc {
            report.requires_restart.push("webrtc".into());
        }
        if old.moq != new_cfg.moq {
            report.requires_restart.push("moq".into());
        }
        if old.hls != new_cfg.hls {
            report.requires_restart.push("hls".into());
        }
        if old.record != new_cfg.record {
            report.requires_restart.push("record".into());
        }
        if old.buffer != new_cfg.buffer {
            report.requires_restart.push("buffer".into());
        }
        // Admin login and health alerts are built once at startup (sessions,
        // the gate middleware, the watcher task); a change needs a restart.
        if old.admin != new_cfg.admin {
            report.requires_restart.push("admin".into());
        }
        if old.health != new_cfg.health {
            report.requires_restart.push("health".into());
        }
        if old.cluster != new_cfg.cluster {
            report.requires_restart.push("cluster".into());
        }

        *current = new_cfg;
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use caudal_core::{Access, Denied, Gate};

    #[tokio::test]
    async fn cluster_tokens_may_play_past_auth() {
        let cluster = caudal_cluster::Secret::new("the-cluster-secret").unwrap();
        let auth = caudal_auth::AuthConfig {
            keys: Some(caudal_auth::KeySource::Secret("0123456789abcdef0123456789abcdef".into())),
            publish: true,
            play: true,
        };
        let access = caudal_access::AccessConfig { rules: Vec::new(), geoip_db: None }.checker().unwrap();
        let gate = CombinedGate { auth: caudal_auth::Authorizer::new(auth), access, cluster: Some(cluster.clone()) };

        let node = cluster.mint("edge-1");
        assert_eq!(gate.check(Access::Play, "cam", Some(&node), None).await, Ok(()));
        // Only play: a node token never publishes.
        assert!(gate.check(Access::Publish, "cam", Some(&node), None).await.is_err());
        let other = caudal_cluster::Secret::new("some-other-secret!").unwrap().mint("edge-1");
        assert!(matches!(gate.check(Access::Play, "cam", Some(&other), None).await, Err(Denied::Refused(_))));
        assert_eq!(gate.check(Access::Play, "cam", None, None).await, Err(Denied::Missing));
    }
}
