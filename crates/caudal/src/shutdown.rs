//! Graceful shutdown signal for axum's `with_graceful_shutdown`: resolves on
//! Ctrl-C, or SIGTERM on Unix, or the console closing / logoff / system
//! shutdown on Windows. Whichever comes first.

/// Waits for a shutdown signal (Ctrl-C or SIGTERM). Pass the returned future
/// to `axum::serve(..).with_graceful_shutdown(...)`.
pub async fn signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };

    // Windows: the console window closing, the user logging off, or the
    // machine shutting down (a service wrapper such as NSSM stops Caudal
    // with Ctrl-C, handled above). Without these Windows kills the process
    // a few seconds later with no clean shutdown.
    #[cfg(windows)]
    let terminate = async {
        use tokio::signal::windows;
        match (windows::ctrl_close(), windows::ctrl_shutdown(), windows::ctrl_logoff()) {
            (Ok(mut close), Ok(mut shutdown), Ok(mut logoff)) => {
                tokio::select! {
                    _ = close.recv() => {}
                    _ = shutdown.recv() => {}
                    _ = logoff.recv() => {}
                }
            }
            _ => {
                tracing::warn!("failed to install the Windows console close/shutdown handlers");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(any(unix, windows)))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received Ctrl-C, shutting down"),
        _ = terminate => tracing::info!("received a terminate signal (SIGTERM / console close / shutdown), shutting down"),
    }
}
