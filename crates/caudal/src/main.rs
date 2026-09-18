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

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
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

    tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("build tokio runtime").block_on(run(cfg))
}

async fn run(cfg: config::Config) -> ExitCode {
    let registry = caudal_core::Registry::new();

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

    // The LL-HLS router is built synchronously; until agent C lands it is a
    // `todo!()` stub, so a panic building it is caught and swapped for an
    // empty router rather than taking the whole server down.
    let hls_cfg = caudal_hls::HlsConfig { part_ms: cfg.hls.part_ms, segment_ms: cfg.hls.segment_ms };
    let hls_registry = registry.clone();
    let hls_router = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        caudal_hls::router(hls_registry, hls_cfg)
    }))
    .unwrap_or_else(|payload| {
        tracing::error!(error = %panic_message(&*payload), "caudal_hls::router panicked (not yet implemented?)");
        axum::Router::new()
    });

    let state = api::AppState::new(registry.clone());
    let app = api::router(state.clone()).merge(hls_router);

    let listener = match tokio::net::TcpListener::bind(cfg.server.http_bind).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, bind = %cfg.server.http_bind, "failed to bind http listener");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(bind = %cfg.server.http_bind, "listening");
    state.mark_ready();

    let result = axum::serve(listener, app.into_make_service()).with_graceful_shutdown(shutdown::signal()).await;

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
