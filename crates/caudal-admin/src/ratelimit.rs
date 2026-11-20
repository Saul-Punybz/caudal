//! Per-IP login attempt limit: a fixed window per client address. IPv6
//! clients are grouped by /64, since one host usually owns a whole /64.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// Entries kept before stale windows are swept.
const SWEEP_AT: usize = 10_000;

pub(crate) struct RateLimiter {
    max: u32,
    window: Duration,
    map: Mutex<HashMap<IpAddr, (Instant, u32)>>,
}

impl RateLimiter {
    pub fn new(max: u32, window: Duration) -> Self {
        Self { max, window, map: Mutex::new(HashMap::new()) }
    }

    /// Counts one attempt from `ip`. `Err(retry_after)` once over the limit.
    pub fn hit(&self, ip: IpAddr, now: Instant) -> Result<(), Duration> {
        let key = bucket(ip);
        let mut map = self.map.lock();
        if map.len() >= SWEEP_AT {
            let window = self.window;
            map.retain(|_, (start, _)| now.duration_since(*start) < window);
        }
        let entry = map.entry(key).or_insert((now, 0));
        if now.duration_since(entry.0) >= self.window {
            *entry = (now, 0);
        }
        if entry.1 >= self.max {
            return Err(self.window.saturating_sub(now.duration_since(entry.0)));
        }
        entry.1 += 1;
        Ok(())
    }
}

/// Failed password logins per account name, across every client address:
/// the per-IP limit alone does not stop many addresses guessing one
/// account. Only failures count, and any name is tracked (known or not), so
/// being refused does not reveal which accounts exist. A success clears the
/// name's count.
pub(crate) struct FailureLimiter {
    max: u32,
    window: Duration,
    map: Mutex<HashMap<String, (Instant, u32)>>,
}

impl FailureLimiter {
    pub fn new(max: u32, window: Duration) -> Self {
        Self { max, window, map: Mutex::new(HashMap::new()) }
    }

    /// `Err(retry_after)` while `name` has `max` failures inside the window.
    pub fn check(&self, name: &str, now: Instant) -> Result<(), Duration> {
        let map = self.map.lock();
        match map.get(name) {
            Some((start, n)) if *n >= self.max && now.duration_since(*start) < self.window => {
                Err(self.window.saturating_sub(now.duration_since(*start)))
            }
            _ => Ok(()),
        }
    }

    pub fn failed(&self, name: &str, now: Instant) {
        let mut map = self.map.lock();
        if map.len() >= SWEEP_AT {
            let window = self.window;
            map.retain(|_, (start, _)| now.duration_since(*start) < window);
            if map.len() >= SWEEP_AT {
                map.clear();
            }
        }
        // Names are bounded by the login body limit; keep at most 256 bytes.
        let key: String = name.chars().take(256).collect();
        let e = map.entry(key).or_insert((now, 0));
        if now.duration_since(e.0) >= self.window {
            *e = (now, 0);
        }
        e.1 += 1;
    }

    pub fn succeeded(&self, name: &str) {
        self.map.lock().remove(name);
    }
}

fn bucket(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let s = v6.segments();
                IpAddr::V6(std::net::Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_per_ip_and_resets_after_the_window() {
        let rl = RateLimiter::new(3, Duration::from_secs(60));
        let t0 = Instant::now();
        let a: IpAddr = "203.0.113.7".parse().unwrap();
        let b: IpAddr = "203.0.113.8".parse().unwrap();
        for _ in 0..3 {
            rl.hit(a, t0).unwrap();
        }
        let retry = rl.hit(a, t0 + Duration::from_secs(10)).unwrap_err();
        assert_eq!(retry, Duration::from_secs(50));
        rl.hit(b, t0).expect("another address has its own budget");
        rl.hit(a, t0 + Duration::from_secs(60)).expect("new window");
    }

    #[test]
    fn failures_per_name_lock_it_across_addresses_and_success_clears() {
        let f = FailureLimiter::new(3, Duration::from_secs(900));
        let t0 = Instant::now();
        for _ in 0..3 {
            f.check("admin", t0).unwrap();
            f.failed("admin", t0);
        }
        assert_eq!(f.check("admin", t0 + Duration::from_secs(100)).unwrap_err(), Duration::from_secs(800));
        f.check("ops", t0).expect("other names keep their budget");
        f.check("admin", t0 + Duration::from_secs(900)).expect("the window ends");
        f.failed("ops", t0);
        f.failed("ops", t0);
        f.succeeded("ops");
        f.failed("ops", t0);
        f.failed("ops", t0);
        f.check("ops", t0).expect("a success reset the count");
    }

    #[test]
    fn ipv6_is_limited_per_64() {
        let rl = RateLimiter::new(1, Duration::from_secs(60));
        let t0 = Instant::now();
        rl.hit("2001:db8:1:2::1".parse().unwrap(), t0).unwrap();
        assert!(rl.hit("2001:db8:1:2:ffff::9".parse().unwrap(), t0).is_err(), "same /64");
        rl.hit("2001:db8:1:3::1".parse().unwrap(), t0).unwrap();
        // v4-mapped counts as the v4 address.
        rl.hit("198.51.100.1".parse().unwrap(), t0).unwrap();
        assert!(rl.hit("::ffff:198.51.100.1".parse().unwrap(), t0).is_err());
    }
}
