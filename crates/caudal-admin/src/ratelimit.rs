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
