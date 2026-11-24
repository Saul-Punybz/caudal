//! Keeps the allocator from sitting on memory the server already freed.
//!
//! A live stream frees and reallocates its whole buffer window (tens of MB
//! of frame payloads) each time a publisher reconnects, and every viewer
//! that leaves frees its session buffers, all from whichever runtime
//! thread the task last ran on. The live heap stays bounded (dhat, soak
//! run 35622517287: it never passed its early peak), but glibc malloc kept
//! the freed pages: with a publisher restart and a viewer batch every
//! 2 minutes, RSS grew +31 MB/h (soak run 44952665812, control). Both
//! halves did it on their own (run 44944552859: publisher restarts alone
//! +28.9 MB/h, viewer batches alone +14.5 MB/h).
//!
//! On glibc, a background thread calls `malloc_trim(0)` every
//! [`DEFAULT_EVERY_SECS`] seconds, which returns the free pages of every
//! arena to the OS, not just the top of the main heap. Same run: -63.8 MB/h
//! (RSS fell). `CAUDAL_MALLOC_TRIM_SECS` changes the period; `0` turns it
//! off. A fixed `mmap_threshold` (+21 MB/h) and `MALLOC_ARENA_MAX=2`
//! (+9.0 MB/h) did not fix it.
//!
//! musl builds (the release binaries) use mimalloc instead (main.rs):
//! musl's own malloc grew +724 MB/h under the same churn.

/// The default trim period on glibc.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub const DEFAULT_EVERY_SECS: u64 = 10;

#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn spawn() {
    let secs = match std::env::var("CAUDAL_MALLOC_TRIM_SECS") {
        Ok(v) => match v.trim().parse::<u64>() {
            Ok(n) => n,
            Err(_) => {
                tracing::warn!(value = %v, "CAUDAL_MALLOC_TRIM_SECS is not a whole number of seconds; using the default");
                DEFAULT_EVERY_SECS
            }
        },
        Err(_) => DEFAULT_EVERY_SECS,
    };
    if secs == 0 {
        tracing::info!("malloc_trim off (CAUDAL_MALLOC_TRIM_SECS=0)");
        return;
    }
    let every = std::time::Duration::from_secs(secs);
    let spawned = std::thread::Builder::new().name("malloc-trim".into()).spawn(move || {
        loop {
            std::thread::sleep(every);
            // SAFETY: malloc_trim takes no pointers and is thread-safe.
            unsafe {
                libc::malloc_trim(0);
            }
        }
    });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "could not start the malloc_trim thread");
    }
}

/// Other platforms: nothing to do (musl uses mimalloc, see main.rs).
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn spawn() {}

/// glibc's own count of heap bytes in use and free-but-kept, from
/// `mallinfo2` (glibc 2.33+, hence opt-in: `--features heap-metrics`).
/// For soaks: RSS growing while `in_use` stays flat is the allocator
/// keeping freed memory, not a leak.
#[cfg(all(feature = "heap-metrics", target_os = "linux", target_env = "gnu"))]
pub fn heap() -> Option<(usize, usize)> {
    // SAFETY: mallinfo2 takes no arguments and returns a plain struct.
    let m = unsafe { libc::mallinfo2() };
    Some((m.uordblks + m.hblkhd, m.fordblks))
}

#[cfg(not(all(feature = "heap-metrics", target_os = "linux", target_env = "gnu")))]
pub fn heap() -> Option<(usize, usize)> {
    None
}
