//! Hands memory glibc malloc keeps cached back to the OS, every few
//! seconds, when `CAUDAL_MALLOC_TRIM_SECS` is set. glibc only.
//!
//! Experimental (fix/soak-leak): under publisher and viewer churn the
//! soak's RSS grew while the live heap stayed under its early peak (dhat,
//! run 35622517287), so the growth is memory glibc holds after `free`.
//! `malloc_trim(0)` returns the free pages of every arena, not just the
//! top of the main heap.

#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn spawn_from_env() {
    let Some(secs) = std::env::var("CAUDAL_MALLOC_TRIM_SECS").ok().and_then(|v| v.parse::<u64>().ok()) else {
        return;
    };
    if secs == 0 {
        return;
    }
    tracing::info!(every_secs = secs, "malloc_trim enabled");
    let every = std::time::Duration::from_secs(secs);
    let _ = std::thread::Builder::new().name("malloc-trim".into()).spawn(move || {
        loop {
            std::thread::sleep(every);
            // SAFETY: malloc_trim takes no pointers and is thread-safe.
            unsafe {
                libc::malloc_trim(0);
            }
        }
    });
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn spawn_from_env() {}
