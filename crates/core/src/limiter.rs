//! Global speed limiter (virtual-scheduling token bucket).
//!
//! `reserve(n)` returns how long the caller must wait before it may consume `n` bytes.
//! Reservations are serialised so the *aggregate* rate across all connections of all
//! downloads stays at the configured limit, with no bursts.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct Limiter {
    /// Bytes per second; 0 = unlimited.
    rate: AtomicU64,
    next_free: Mutex<Option<Instant>>,
}

impl Default for Limiter {
    fn default() -> Self {
        Self::new(0)
    }
}

impl Limiter {
    pub fn new(bytes_per_sec: u64) -> Self {
        Self { rate: AtomicU64::new(bytes_per_sec), next_free: Mutex::new(None) }
    }

    pub fn set_rate(&self, bytes_per_sec: u64) {
        self.rate.store(bytes_per_sec, Ordering::Relaxed);
        *self.next_free.lock().unwrap() = None;
    }

    pub fn rate(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }

    pub fn reserve(&self, n: usize) -> Duration {
        let rate = self.rate();
        if rate == 0 || n == 0 {
            return Duration::ZERO;
        }
        let now = Instant::now();
        let mut g = self.next_free.lock().unwrap();
        let start = match *g {
            Some(t) if t > now => t,
            _ => now,
        };
        *g = Some(start + Duration::from_secs_f64(n as f64 / rate as f64));
        start.saturating_duration_since(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_never_waits() {
        let l = Limiter::new(0);
        assert_eq!(l.reserve(10_000_000), Duration::ZERO);
    }

    #[test]
    fn limited_spaces_reservations() {
        let l = Limiter::new(1000); // 1000 B/s
        assert_eq!(l.reserve(500), Duration::ZERO); // first goes immediately
        let w = l.reserve(500); // must wait ~0.5 s for the first 500 B
        assert!(w > Duration::from_millis(400) && w <= Duration::from_millis(500), "{w:?}");
        let w2 = l.reserve(1000);
        assert!(w2 > Duration::from_millis(900) && w2 <= Duration::from_millis(1000), "{w2:?}");
    }
}
