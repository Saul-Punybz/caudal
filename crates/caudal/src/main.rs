//! `caudal`: the server shell. Wires together `caudal-core`'s registry,
//! the RTMP ingest and LL-HLS output crates (owned by other agents), and
//! this crate's own HTTP surface (health, readiness, stream inventory,
//! metrics).

mod admin;
mod api;
mod config;
mod metrics;
mod reload;
mod shutdown;
mod subsystems;

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
    /// Reads a password from stdin and prints its argon2id hash for
    /// `[[admin.users]] password_hash`.
    HashPassword,
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

/// Resolves the config to start with, and the file path (if any) that a
/// later reload should re-read: explicit `--config`, or `caudal.toml` in
/// the current directory if it exists, otherwise built-in defaults with no
/// file to reload from.
fn resolve_config(explicit: Option<PathBuf>) -> Result<(config::Config, Option<PathBuf>), String> {
    let path = explicit.or_else(|| {
        let default_path = PathBuf::from("caudal.toml");
        default_path.exists().then_some(default_path)
    });
    match path {
        Some(path) => config::load(&path).map(|cfg| (cfg, Some(path))),
        None => Ok((config::Config::default(), None)),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    if let Some(Command::HashPassword) = &cli.command {
        return admin::hash_password_cmd();
    }
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

    let (cfg, config_path) = match resolve_config(cli.config) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    init_logging();

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("build tokio runtime");
    let code = rt.block_on(run(cfg, config_path));
    // Dropping a runtime waits forever for blocking tasks (file reads,
    // DNS); a stuck one once kept the process alive after a clean shutdown.
    rt.shutdown_timeout(std::time::Duration::from_secs(5));
    code
}

async fn run(cfg: config::Config, config_path: Option<PathBuf>) -> ExitCode {
    let admin = match admin::setup(&cfg) {
        Ok(a) => a,
        Err(e) => {
            tracing::error!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let registry = caudal_core::Registry::new();

    // Starts RTMP, SRT, RTSP, restream, channels and transcode, and wires
    // auth/webhooks: everything `subsystems::Supervisor::reload` can later
    // apply hot or restart on its own (see that module's docs). Each
    // listener runs in its own task; a panic or I/O error there is logged
    // and does not bring down the HTTP side.
    let started = subsystems::Supervisor::start(&cfg, registry.clone());

    let hls_router = caudal_hls::router(
        registry.clone(),
        caudal_hls::HlsConfig {
            part_ms: cfg.hls.part_ms,
            segment_ms: cfg.hls.segment_ms,
            cue_tags: cfg.hls.cue_tags,
            cue_out_tags: cfg.hls.cue_out_tags,
            reconnect_grace: std::time::Duration::from_secs(cfg.hls.reconnect_grace_secs.into()),
        },
    );

    let state = api::AppState::new(registry.clone());
    let webrtc_router = caudal_webrtc::router(
        registry.clone(),
        caudal_webrtc::WebRtcConfig {
            udp_bind: cfg.webrtc.udp_bind,
            public_ips: cfg.webrtc.public_ips.clone(),
            buffer: cfg.buffer.to_buffer_config(),
            threads: cfg.webrtc.threads,
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

    let reload_state = reload::ReloadState { supervisor: started.supervisor.clone(), config_path };
    reload::spawn_sighup(reload_state.clone());

    // Stream health alerts: no keyframes, bitrate floor, publisher lost.
    // Disabled (no router, no background task) unless `[health]` names at
    // least one webhook.
    let health_router = match cfg.health.to_health_config().expect("validated") {
        Some(hc) => {
            let webhooks = hc.webhooks.len();
            match caudal_health::start(registry.clone(), hc) {
                Ok(svc) => {
                    tracing::info!(webhooks, "stream health alerts enabled");
                    let svc = std::sync::Arc::new(svc);
                    state.set_health(svc.clone());
                    svc.router()
                }
                Err(e) => {
                    tracing::error!(error = %e, "stream health alerts disabled");
                    axum::Router::new()
                }
            }
        }
        None => axum::Router::new(),
    };

    // The UI router is a catch-all fallback, so it goes last.
    let app = api::router(state.clone())
        .merge(hls_router)
        .merge(webrtc_router)
        .merge(moq_router)
        .merge(record_router)
        .merge(started.channel_router)
        .merge(started.restream_router)
        .merge(reload::router(reload_state))
        .merge(health_router)
        .merge(caudal_ui::router());
    let app = admin::wrap(app, admin);

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
            let tls_app = app.clone().layer(axum::Extension(caudal_admin::ViaTls));
            Some(tokio::spawn(caudal_tls::serve(tls, tls_app, stopped(stop_rx.clone()))))
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
