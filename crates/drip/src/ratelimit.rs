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

    /// Non-mutating peek at an address's rate-limit state. Returns the
    /// same `RateCheck` shape as `check`, but never advances state and
    /// never inserts a new entry. Used by `GET /v1/drip/status?address=`
    /// so the explorer can render a cooldown badge without burning a
    /// window slot.
    ///
    /// Today's `check` is already non-mutating in practice (it only
    /// reads `last_drip`, and the actual recording happens via the
    /// separate `record` call). `peek` exists as a distinct read-only
    /// affordance so future evolutions of `check` (e.g. tentative
    /// reservations, sliding-window counters) can mutate without
    /// breaking the read endpoint.
    ///
    /// # Examples
    ///
    /// A fresh address peeks `Allowed` and `peek` doesn't insert any
    /// state. The same address peeks `Allowed` again, and the
    /// distinct-address counter stays at zero:
    ///
    /// ```
    /// use std::time::Duration;
    /// use ligate_api_drip::{RateCheck, RateLimiter};
    ///
    /// let rl = RateLimiter::new(Duration::from_secs(3600));
    /// assert!(matches!(rl.peek("lig1fresh"), RateCheck::Allowed));
    /// assert!(matches!(rl.peek("lig1fresh"), RateCheck::Allowed));
    /// assert_eq!(rl.drip_count(), 0);
    /// ```
    ///
    /// After `record`, peek reports `Blocked` with a positive
    /// `retry_after`. The per-address branch of
    /// `GET /v1/drip/status?address=` reads that and renders the
    /// cooldown boundary in absolute time:
    ///
    /// ```
    /// use std::time::Duration;
    /// use ligate_api_drip::{RateCheck, RateLimiter};
    ///
    /// let rl = RateLimiter::new(Duration::from_secs(3600));
    /// rl.record("lig1dripped");
    /// match rl.peek("lig1dripped") {
    ///     RateCheck::Blocked { retry_after } => {
    ///         assert!(retry_after.as_secs() > 0);
    ///         assert!(retry_after.as_secs() <= 3600);
    ///     }
    ///     RateCheck::Allowed => panic!("expected cooldown"),
    /// }
    /// ```
    pub fn peek(&self, address: &str) -> RateCheck {
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

    #[test]
    fn peek_does_not_burn_window() {
        // A fresh address peeks `Allowed` and remains `Allowed` after
        // an arbitrary number of peeks. Peek never inserts a row
        // into `last_drip`, so repeated peeks can't accidentally
        // start a cooldown.
        let rl = RateLimiter::new(Duration::from_secs(60));
        assert!(matches!(rl.peek("lig1abc"), RateCheck::Allowed));
        assert!(matches!(rl.peek("lig1abc"), RateCheck::Allowed));
        assert!(matches!(rl.peek("lig1abc"), RateCheck::Allowed));
        assert_eq!(rl.drip_count(), 0);
    }

    #[test]
    fn peek_reflects_recorded_state() {
        // After `record`, peek surfaces the same `Blocked` state that
        // `check` would, with the residual `retry_after` for cooldown
        // rendering.
        let rl = RateLimiter::new(Duration::from_secs(60));
        rl.record("lig1abc");
        assert!(matches!(rl.peek("lig1abc"), RateCheck::Blocked { .. }));
    }
}
