//! Standard `process_*` Prometheus metrics for the server's own process,
//! read from the OS on each scrape with no extra dependency beyond `libc`
//! (Unix) and `windows-sys` (Windows), both already in the tree.
//!
//! A value the platform cannot give is `None` and its metric is left out of
//! the output: never zero, never estimated.
//!
//! | metric | Linux | macOS | Windows |
//! |---|---|---|---|
//! | `process_cpu_seconds_total` | /proc | getrusage | GetProcessTimes |
//! | `process_resident_memory_bytes` | /proc | proc_pidinfo | working set |
//! | `process_virtual_memory_bytes` | /proc | proc_pidinfo | not exposed |
//! | `process_open_fds` | /proc/self/fd | proc_pidinfo | handle count |
//! | `process_start_time_seconds` | /proc | proc_pidinfo | GetProcessTimes |
//! | `process_threads` | /proc | proc_pidinfo | not exposed |

use std::fmt::Write as _;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct ProcessStats {
    /// User plus system CPU time.
    pub cpu_seconds: Option<f64>,
    pub resident_bytes: Option<u64>,
    pub virtual_bytes: Option<u64>,
    pub open_fds: Option<u64>,
    /// Unix seconds.
    pub start_time_seconds: Option<f64>,
    pub threads: Option<u64>,
}

/// Reads the current numbers.
pub fn collect() -> ProcessStats {
    imp::collect()
}

/// Appends the `process_*` metrics for `stats` in the text format.
pub fn render(out: &mut String, stats: &ProcessStats) {
    fn one(out: &mut String, name: &str, help: &str, kind: &str, v: Option<String>) {
        if let Some(v) = v {
            let _ = writeln!(out, "# HELP {name} {help}");
            let _ = writeln!(out, "# TYPE {name} {kind}");
            let _ = writeln!(out, "{name} {v}");
        }
    }
    one(
        out,
        "process_cpu_seconds_total",
        "Total user and system CPU time spent in seconds.",
        "counter",
        stats.cpu_seconds.map(|v| format!("{v:.3}")),
    );
    one(
        out,
        "process_resident_memory_bytes",
        "Resident memory size in bytes.",
        "gauge",
        stats.resident_bytes.map(|v| v.to_string()),
    );
    one(
        out,
        "process_virtual_memory_bytes",
        "Virtual memory size in bytes.",
        "gauge",
        stats.virtual_bytes.map(|v| v.to_string()),
    );
    one(out, "process_open_fds", "Number of open file descriptors.", "gauge", stats.open_fds.map(|v| v.to_string()));
    one(
        out,
        "process_start_time_seconds",
        "Start time of the process since the unix epoch in seconds.",
        "gauge",
        stats.start_time_seconds.map(|v| format!("{v:.0}")),
    );
    one(out, "process_threads", "Number of OS threads in the process.", "gauge", stats.threads.map(|v| v.to_string()));
}

#[cfg(target_os = "linux")]
mod imp {
    use super::ProcessStats;

    pub fn collect() -> ProcessStats {
        let mut s = ProcessStats::default();
        // SAFETY: sysconf has no preconditions.
        let (clk, page) = unsafe { (libc::sysconf(libc::_SC_CLK_TCK), libc::sysconf(libc::_SC_PAGESIZE)) };
        let (clk, page) = (clk.max(0) as u64, page.max(0) as u64);
        if let Ok(stat) = std::fs::read_to_string("/proc/self/stat") {
            // Field 2 (comm) may hold spaces and parens: split after the last ')'.
            if let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) {
                let f: Vec<&str> = rest.split_whitespace().collect();
                // `rest` starts at field 3 (state), so field N is f[N - 3].
                let get = |n: usize| f.get(n - 3).and_then(|v| v.parse::<u64>().ok());
                if clk > 0 {
                    if let (Some(u), Some(k)) = (get(14), get(15)) {
                        s.cpu_seconds = Some((u + k) as f64 / clk as f64);
                    }
                    if let (Some(start), Some(btime)) = (get(22), boot_time()) {
                        s.start_time_seconds = Some(btime as f64 + start as f64 / clk as f64);
                    }
                }
                s.threads = get(20);
                s.virtual_bytes = get(23);
                if page > 0 {
                    s.resident_bytes = get(24).map(|p| p * page);
                }
            }
        }
        s.open_fds = std::fs::read_dir("/proc/self/fd").ok().map(|d| d.count() as u64);
        s
    }

    fn boot_time() -> Option<u64> {
        let stat = std::fs::read_to_string("/proc/stat").ok()?;
        stat.lines().find_map(|l| l.strip_prefix("btime ")?.trim().parse().ok())
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::ProcessStats;
    use std::mem::{size_of, zeroed};

    pub fn collect() -> ProcessStats {
        let mut s = ProcessStats::default();
        // SAFETY: getrusage and proc_pidinfo fill zeroed structs we own,
        // with the sizes passed alongside.
        unsafe {
            let mut ru: libc::rusage = zeroed();
            if libc::getrusage(libc::RUSAGE_SELF, &mut ru) == 0 {
                let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
                s.cpu_seconds = Some(tv(ru.ru_utime) + tv(ru.ru_stime));
            }
            let pid = libc::getpid();
            let mut ti: libc::proc_taskinfo = zeroed();
            let n = libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTASKINFO,
                0,
                (&mut ti as *mut libc::proc_taskinfo).cast(),
                size_of::<libc::proc_taskinfo>() as libc::c_int,
            );
            if n == size_of::<libc::proc_taskinfo>() as libc::c_int {
                s.resident_bytes = Some(ti.pti_resident_size);
                s.virtual_bytes = Some(ti.pti_virtual_size);
                s.threads = u64::try_from(ti.pti_threadnum).ok();
            }
            let mut bi: libc::proc_bsdinfo = zeroed();
            let n = libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut bi as *mut libc::proc_bsdinfo).cast(),
                size_of::<libc::proc_bsdinfo>() as libc::c_int,
            );
            if n == size_of::<libc::proc_bsdinfo>() as libc::c_int {
                s.start_time_seconds = Some(bi.pbi_start_tvsec as f64 + bi.pbi_start_tvusec as f64 / 1e6);
            }
            // With a null buffer the call returns the bytes the list would need.
            let n = libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0);
            if n >= 0 {
                s.open_fds = Some(n as u64 / size_of::<libc::proc_fdinfo>() as u64);
            }
        }
        s
    }
}

#[cfg(windows)]
mod imp {
    use super::ProcessStats;
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount, GetProcessTimes};

    fn ticks(ft: FILETIME) -> u64 {
        (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime)
    }

    pub fn collect() -> ProcessStats {
        let mut s = ProcessStats::default();
        // SAFETY: the pseudo handle is always valid; every out-struct is
        // zeroed, owned here and sized for the call.
        unsafe {
            let h = GetCurrentProcess();
            let (mut created, mut exit, mut kernel, mut user): (FILETIME, FILETIME, FILETIME, FILETIME) =
                (zeroed(), zeroed(), zeroed(), zeroed());
            if GetProcessTimes(h, &mut created, &mut exit, &mut kernel, &mut user) != 0 {
                // FILETIME ticks are 100 ns; creation is since 1601-01-01.
                s.cpu_seconds = Some((ticks(kernel) + ticks(user)) as f64 / 1e7);
                s.start_time_seconds = Some(ticks(created) as f64 / 1e7 - 11_644_473_600.0);
            }
            let mut mem: PROCESS_MEMORY_COUNTERS = zeroed();
            mem.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            if GetProcessMemoryInfo(h, &mut mem, mem.cb) != 0 {
                s.resident_bytes = Some(mem.WorkingSetSize as u64);
            }
            let mut handles = 0u32;
            if GetProcessHandleCount(h, &mut handles) != 0 {
                s.open_fds = Some(u64::from(handles));
            }
        }
        s
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod imp {
    use super::ProcessStats;

    pub fn collect() -> ProcessStats {
        ProcessStats::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn collect_is_plausible_on_this_platform() {
        // Burn a little CPU so the counter is nonzero on coarse clocks.
        let mut x = 0u64;
        for i in 0..20_000_000u64 {
            x = x.wrapping_add(i * i);
        }
        std::hint::black_box(x);
        let s = collect();
        assert!(s.resident_bytes.expect("rss available") > 0);
        assert!(s.cpu_seconds.expect("cpu available") > 0.0);
        assert!(s.open_fds.expect("fds available") > 0);
        let start = s.start_time_seconds.expect("start time available");
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
        assert!(start > 1.5e9 && start <= now + 1.0, "start {start} vs now {now}");
        #[cfg(not(windows))]
        assert!(s.threads.expect("threads available") >= 1);
        #[cfg(not(windows))]
        assert!(s.virtual_bytes.expect("virtual available") > 0);
    }

    #[test]
    fn render_names_types_and_omits_missing() {
        let s = ProcessStats {
            cpu_seconds: Some(1.5),
            resident_bytes: Some(4096),
            virtual_bytes: None,
            open_fds: Some(12),
            start_time_seconds: Some(1_700_000_000.0),
            threads: None,
        };
        let mut out = String::new();
        render(&mut out, &s);
        assert!(out.contains("# TYPE process_cpu_seconds_total counter\nprocess_cpu_seconds_total 1.500\n"));
        assert!(out.contains("# TYPE process_resident_memory_bytes gauge\nprocess_resident_memory_bytes 4096\n"));
        assert!(out.contains("# TYPE process_open_fds gauge\nprocess_open_fds 12\n"));
        assert!(out.contains("process_start_time_seconds 1700000000\n"));
        assert!(!out.contains("process_virtual_memory_bytes"), "unavailable means omitted");
        assert!(!out.contains("process_threads"));
    }
}
