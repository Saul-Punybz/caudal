//! `caudal`: the server shell. Wires together `caudal-core`'s registry,
//! the RTMP ingest and LL-HLS output crates (owned by other agents), and
//! this crate's own HTTP surface (health, readiness, stream inventory,
//! metrics).

mod api;
mod config;
mod metrics;
mod shutdown;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "caudal", about = "Caudal media server", version)]
struct Cli {
    /// Path to the TOML config file. Defaults to `caudal.toml` in the
    /// current directory if it exists, otherwise built-in defaults.
    #[arg(long)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Validates a config file and exits non-zero naming the bad key.
    Check { path: PathBuf },
}

fn make_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
}

/// JSON logs when `CAUDAL_LOG_FORMAT=json`, pretty otherwise. Level from
/// `RUST_LOG`, default `info`.
fn init_logging() {
    let json = std::env::var("CAUDAL_LOG_FORMAT").as_deref() == Ok("json");
    if json {
        tracing_subscriber::fmt().with_env_filter(make_filter()).json().init();
    } else {
        tracing_subscriber::fmt().with_env_filter(make_filter()).init();
    }
}

fn resolve_config(explicit: Option<PathBuf>) -> Result<config::Config, String> {
    match explicit {
        Some(path) => config::load(&path),
        None => {
            let default_path = PathBuf::from("caudal.toml");
            if default_path.exists() { config::load(&default_path) } else { Ok(config::Config::default()) }
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    if let Some(Command::Check { path }) = &cli.command {
        return match config::load(path) {
            Ok(_) => {
                println!("ok");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        };
    }

    let cfg = match resolve_config(cli.config) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    init_logging();

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("build tokio runtime");
    let code = rt.block_on(run(cfg));
    // Dropping a runtime waits forever for blocking tasks (file reads,
    // DNS); a stuck one once kept the process alive after a clean shutdown.
    rt.shutdown_timeout(std::time::Duration::from_secs(5));
    code
}

/// Plugs `caudal-auth` into the core's access gate.
struct AuthGate(caudal_auth::Authorizer);

impl caudal_core::Gate for AuthGate {
    fn check<'a>(
        &'a self,
        access: caudal_core::Access,
        stream: &'a str,
        token: Option<&'a str>,
    ) -> caudal_core::GateFuture<'a> {
        Box::pin(async move {
            let action = match access {
                caudal_core::Access::Publish => caudal_auth::Action::Publish,
                caudal_core::Access::Play => caudal_auth::Action::Play,
            };
            self.0.check(action, stream, token).await.map_err(|e| match e {
                caudal_auth::AuthError::Missing => caudal_core::Denied::Missing,
                other => caudal_core::Denied::Refused(other.to_string()),
            })
        })
    }
}

/// Sends a webhook for every stream that starts or ends.
fn spawn_hooks(registry: &std::sync::Arc<caudal_core::Registry>, hooks: caudal_auth::Hooks) {
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
    });
}

async fn run(cfg: config::Config) -> ExitCode {
    let registry = caudal_core::Registry::new();

    // Validated at load; unwraps below cannot fail.
    let auth = cfg.auth.to_auth_config().expect("validated");
    if auth.keys.is_some() {
        tracing::info!(publish = auth.publish, play = auth.play, "token auth enabled");
        registry.set_gate(std::sync::Arc::new(AuthGate(caudal_auth::Authorizer::new(auth))));
    } else {
        tracing::warn!("no [auth] keys: anyone who can reach the server can publish and play");
    }
    if let Some(hooks) = cfg.hooks.to_hooks_config().expect("validated") {
        spawn_hooks(&registry, caudal_auth::Hooks::new(Some(hooks)));
    }

    // RTMP ingest runs in its own task; a panic or I/O error there is logged
    // and does not bring down the HTTP side.
    let rtmp_cfg = caudal_rtmp::RtmpConfig {
        bind: cfg.rtmp.bind,
        app: cfg.rtmp.app.clone(),
        buffer: cfg.buffer.to_buffer_config(),
    };
    tracing::info!(bind = %rtmp_cfg.bind, app = %rtmp_cfg.app, "starting rtmp listener");
    let rtmp_handle = tokio::spawn(caudal_rtmp::serve(rtmp_cfg, registry.clone()));
    tokio::spawn(async move {
        match rtmp_handle.await {
            Ok(Ok(())) => tracing::info!("rtmp listener stopped"),
            Ok(Err(e)) => tracing::error!(error = %e, "rtmp listener failed"),
            Err(join_err) => tracing::error!(error = %join_err, "rtmp task panicked"),
        }
    });

    let srt_cfg = caudal_srt::SrtConfig {
        bind: cfg.srt.bind,
        latency_ms: cfg.srt.latency_ms,
        passphrase: cfg.srt.passphrase.clone(),
        buffer: cfg.buffer.to_buffer_config(),
        pushes: cfg
            .srt
            .push
            .iter()
            .map(|p| caudal_srt::SrtPush { stream: p.stream.clone(), url: p.url.clone() })
            .collect(),
    };
    tracing::info!(bind = %srt_cfg.bind, "starting srt listener");
    let srt_handle = tokio::spawn(caudal_srt::serve(srt_cfg, registry.clone()));
    tokio::spawn(async move {
        match srt_handle.await {
            Ok(Ok(())) => tracing::info!("srt listener stopped"),
            Ok(Err(e)) => tracing::error!(error = %e, "srt listener failed"),
            Err(join_err) => tracing::error!(error = %join_err, "srt task panicked"),
        }
    });

    let rtsp_cfg = caudal_rtsp::RtspConfig {
        bind: cfg.rtsp.bind,
        pulls: cfg
            .rtsp
            .pull
            .iter()
            .map(|p| caudal_rtsp::RtspPull { stream: p.stream.clone(), url: p.url.clone() })
            .collect(),
        buffer: cfg.buffer.to_buffer_config(),
    };
    if rtsp_cfg.bind.is_some() || !rtsp_cfg.pulls.is_empty() {
        let rtsp_handle = tokio::spawn(caudal_rtsp::serve(rtsp_cfg, registry.clone()));
        tokio::spawn(async move {
            match rtsp_handle.await {
                Ok(Ok(())) => tracing::info!("rtsp stopped"),
                Ok(Err(e)) => tracing::error!(error = %e, "rtsp failed"),
                Err(e) => tracing::error!(error = %e, "rtsp task panicked"),
            }
        });
    }

    if let Some(tc) = cfg.transcode.to_transcode_config(cfg.buffer.to_buffer_config()).expect("validated") {
        if let Err(e) = caudal_transcode::start(registry.clone(), tc) {
            tracing::error!(error = %e, "transcoding disabled");
        }
    }

    let hls_router = caudal_hls::router(
        registry.clone(),
        caudal_hls::HlsConfig { part_ms: cfg.hls.part_ms, segment_ms: cfg.hls.segment_ms, cue_tags: cfg.hls.cue_tags },
    );

    let state = api::AppState::new(registry.clone());
    let webrtc_router = caudal_webrtc::router(
        registry.clone(),
        caudal_webrtc::WebRtcConfig {
            udp_bind: cfg.webrtc.udp_bind,
            public_ips: cfg.webrtc.public_ips.clone(),
            buffer: cfg.buffer.to_buffer_config(),
        },
    );

    // MoQ failing to start (e.g. its UDP port is taken) disables MoQ only.
    let moq_router = match cfg.moq.to_moq_config().expect("validated") {
        Some(moq) => {
            let bind = moq.bind;
            match caudal_moq::start(registry.clone(), moq) {
                Ok(svc) => {
                    tracing::info!(%bind, "moq listening (QUIC / WebTransport)");
                    svc.router()
                }
                Err(e) => {
                    tracing::error!(error = %e, %bind, "moq output disabled");
                    axum::Router::new()
                }
            }
        }
        None => axum::Router::new(),
    };

    // Recording failing to start (e.g. its directory is not writable)
    // disables recording only.
    let record_router = match cfg.record.to_record_config() {
        Some(rc) => {
            let dir = rc.dir.display().to_string();
            match caudal_record::start(registry.clone(), rc) {
                Ok(svc) => {
                    tracing::info!(%dir, "recording enabled");
                    svc.router()
                }
                Err(e) => {
                    tracing::error!(error = %e, %dir, "recording disabled");
                    axum::Router::new()
                }
            }
        }
        None => axum::Router::new(),
    };

    let channel_router = if cfg.channel.is_empty() {
        axum::Router::new()
    } else {
        let channels = cfg
            .channel
            .iter()
            .map(|c| caudal_channel::Channel {
                name: c.name.clone(),
                items: c.items.clone(),
                r#loop: c.r#loop,
                shuffle: c.shuffle,
            })
            .collect::<Vec<_>>();
        tracing::info!(channels = channels.len(), "24/7 channels enabled");
        let handle = caudal_channel::start(
            registry.clone(),
            caudal_channel::ChannelConfig { channels, buffer: cfg.buffer.to_buffer_config() },
        );
        caudal_channel::router(handle)
    };

    let restream_router = if cfg.restream.is_empty() {
        axum::Router::new()
    } else {
        let targets = cfg
            .restream
            .iter()
            .map(|r| caudal_restream::RestreamTarget { stream: r.stream.clone(), url: r.url.clone() })
            .collect::<Vec<_>>();
        tracing::info!(targets = targets.len(), "multistreaming enabled");
        let handle = caudal_restream::start(registry.clone(), caudal_restream::RestreamConfig { targets });
        caudal_restream::router(handle)
    };

    // The UI router is a catch-all fallback, so it goes last.
    let app = api::router(state.clone())
        .merge(hls_router)
        .merge(webrtc_router)
        .merge(moq_router)
        .merge(record_router)
        .merge(channel_router)
        .merge(restream_router)
        .merge(caudal_ui::router());

    let listener = match tokio::net::TcpListener::bind(cfg.server.http_bind).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, bind = %cfg.server.http_bind, "failed to bind http listener");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(bind = %cfg.server.http_bind, "listening");

    // One signal, many servers: Ctrl-C / SIGTERM flips this, both drain.
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown::signal().await;
        let _ = stop_tx.send(true);
    });
    let stopped = |mut rx: tokio::sync::watch::Receiver<bool>| async move {
        let _ = rx.wait_for(|stop| *stop).await;
    };

    let tls_task = match cfg.tls.to_tls_config().expect("validated") {
        Some(tls) => {
            tracing::info!(bind = %tls.bind, "https listening (HTTP/1.1 + HTTP/2)");
            Some(tokio::spawn(caudal_tls::serve(tls, app.clone(), stopped(stop_rx.clone()))))
        }
        None => None,
    };
    state.mark_ready();

    let result = axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>())
        .with_graceful_shutdown(stopped(stop_rx))
        .await;
    if let Some(t) = tls_task {
        match t.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::error!(error = %e, "https server failed"),
            Err(e) => tracing::error!(error = %e, "https task panicked"),
        }
    }

    match result {
        Ok(()) => {
            tracing::info!("shut down cleanly");
            ExitCode::SUCCESS
        }
        Err(e) => {
            tracing::error!(error = %e, "server error");
            ExitCode::FAILURE
        }
    }
}
