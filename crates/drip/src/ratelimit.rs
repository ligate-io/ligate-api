//! In-memory per-address rate limiter.
//!
//! Trades durability for simplicity: drip history lives in process
//! memory only. Faucet restart resets the window for everyone (which
//! is the right thing for v0; operators don't restart the faucet
//! mid-day under normal conditions).
//!
//! Per-IP rate limiting is intentionally NOT here. Adversaries can
//! trivially rotate IPs (any cloud provider, any VPN) so per-IP
//! limits buy little. The per-address limit is the substantive
//! defense: each `lig1...` address only gets one drip per window,
//! independent of how many requests came from how many IPs.

use std::time::{Duration, Instant};

use dashmap::DashMap;

pub struct RateLimiter {
    last_drip: DashMap<String, Instant>,
    window: Duration,
}

#[derive(Debug, Clone, Copy)]
pub enum RateCheck {
    Allowed,
    Blocked { retry_after: Duration },
}

impl RateLimiter {
    pub fn new(window: Duration) -> Self {
        Self {
            last_drip: DashMap::new(),
            window,
        }
    }

    /// Check whether an address is allowed to drip right now. Does
    /// NOT record the drip; call `record` after a successful drip
    /// (so failed signer attempts don't burn the address's window).
    pub fn check(&self, address: &str) -> RateCheck {
        let now = Instant::now();
        match self.last_drip.get(address) {
            Some(last) => {
                let elapsed = now.duration_since(*last);
                if elapsed >= self.window {
                    RateCheck::Allowed
                } else {
                    RateCheck::Blocked {
                        retry_after: self.window - elapsed,
                    }
                }
            }
            None => RateCheck::Allowed,
        }
    }

    /// Record a successful drip. Call only after the chain has
    /// accepted the transaction (to prevent failed-submission grief).
    pub fn record(&self, address: &str) {
        self.last_drip.insert(address.to_string(), Instant::now());
    }

    /// Approximate count of distinct addresses dripped at least once.
    /// O(1) read for the status endpoint.
    pub fn drip_count(&self) -> usize {
        self.last_drip.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn first_call_allowed() {
        let rl = RateLimiter::new(Duration::from_secs(60));
        assert!(matches!(rl.check("lig1abc"), RateCheck::Allowed));
    }

    #[test]
    fn second_call_within_window_blocked() {
        let rl = RateLimiter::new(Duration::from_secs(60));
        rl.record("lig1abc");
        assert!(matches!(rl.check("lig1abc"), RateCheck::Blocked { .. }));
    }

    #[test]
    fn different_addresses_independent() {
        let rl = RateLimiter::new(Duration::from_secs(60));
        rl.record("lig1abc");
        assert!(matches!(rl.check("lig1xyz"), RateCheck::Allowed));
    }

    #[test]
    fn drip_count_tracks_records() {
        let rl = RateLimiter::new(Duration::from_secs(60));
        assert_eq!(rl.drip_count(), 0);
        rl.record("lig1abc");
        rl.record("lig1xyz");
        rl.record("lig1abc"); // same address; replaces, doesn't add
        assert_eq!(rl.drip_count(), 2);
    }

    #[test]
    fn after_window_allowed() {
        let rl = RateLimiter::new(Duration::from_millis(10));
        rl.record("lig1abc");
        thread::sleep(Duration::from_millis(15));
        assert!(matches!(rl.check("lig1abc"), RateCheck::Allowed));
    }
}
