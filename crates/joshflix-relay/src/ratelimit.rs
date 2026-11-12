//! Fixed-window, per-IP rate limiting. Small and good enough for a LAN.

use std::{
    collections::HashMap,
    net::IpAddr,
    sync::Mutex,
    time::{Duration, Instant},
};

use crate::netguard::canonical_ip;

/// At most `max` events per `per`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limit {
    pub max: u32,
    pub per: Duration,
}

/// Entries kept before a check forces a sweep. Bounds memory if many peers appear.
const SWEEP_AT: usize = 4096;

pub struct RateLimiter {
    limit: Limit,
    windows: Mutex<HashMap<IpAddr, (Instant, u32)>>,
}

impl RateLimiter {
    pub fn new(limit: Limit) -> Self {
        Self {
            limit,
            windows: Mutex::new(HashMap::new()),
        }
    }

    /// Records one event for `ip`. Returns false when the limit is exceeded.
    pub fn check(&self, ip: IpAddr) -> bool {
        let ip = canonical_ip(ip);
        let now = Instant::now();
        let mut windows = self.windows.lock().unwrap_or_else(|p| p.into_inner());
        if windows.len() >= SWEEP_AT {
            let per = self.limit.per;
            windows.retain(|_, (start, _)| now.duration_since(*start) < per);
        }
        let entry = windows.entry(ip).or_insert((now, 0));
        if now.duration_since(entry.0) >= self.limit.per {
            *entry = (now, 0);
        }
        if entry.1 >= self.limit.max {
            return false;
        }
        entry.1 += 1;
        true
    }

    /// Forgets windows that have ended.
    pub fn sweep(&self) {
        let now = Instant::now();
        let per = self.limit.per;
        self.windows
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|_, (start, _)| now.duration_since(*start) < per);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_per_ip_and_resets() {
        let limiter = RateLimiter::new(Limit {
            max: 2,
            per: Duration::from_millis(50),
        });
        let a: IpAddr = "192.168.1.2".parse().expect("ip");
        let b: IpAddr = "192.168.1.3".parse().expect("ip");
        assert!(limiter.check(a));
        assert!(limiter.check(a));
        assert!(!limiter.check(a));
        // Mapped form of the same address shares the window.
        assert!(!limiter.check("::ffff:192.168.1.2".parse().expect("ip")));
        assert!(limiter.check(b));
        std::thread::sleep(Duration::from_millis(60));
        assert!(limiter.check(a));
        limiter.sweep();
    }
}
