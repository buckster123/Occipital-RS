//! Per-domain rate limiting — the testable core of the politeness contract.
//!
//! A request to a domain may not go out until `interval` (= `1 / rate_per_sec`)
//! has passed since the previous one was *scheduled*. Concurrent callers to the
//! same domain queue: each reserves the next slot, so spacing holds even under
//! parallelism.
//!
//! **Jitter is additive-only** — `interval + rand(0..jitter·interval)`, never
//! `±`. This breaks the metronome pattern (the #1 bot tell) while guaranteeing
//! the spacing is *always ≥ interval*: politeness is a floor, not an average.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rand::Rng;

pub struct DomainLimiter {
    interval:    Duration,
    jitter_frac: f64,
    /// domain key → earliest `Instant` the next request to it may be scheduled.
    next_allowed: Mutex<HashMap<String, Instant>>,
}

impl DomainLimiter {
    /// `rate_per_sec` ≤ 0 disables limiting (interval 0). `jitter_frac` is the
    /// fraction of `interval` added at random on top (e.g. 0.3 = up to +30%).
    pub fn new(rate_per_sec: f64, jitter_frac: f64) -> Self {
        let interval = if rate_per_sec > 0.0 {
            Duration::from_secs_f64(1.0 / rate_per_sec)
        } else {
            Duration::ZERO
        };
        Self {
            interval,
            jitter_frac: jitter_frac.max(0.0),
            next_allowed: Mutex::new(HashMap::new()),
        }
    }

    /// Block until it is polite to hit `key` again, reserving this request's slot.
    pub async fn throttle(&self, key: &str) {
        if self.interval.is_zero() {
            return;
        }
        let wait = {
            // Short, await-free critical section (std Mutex is correct here).
            let mut map = self.next_allowed.lock().unwrap();
            let now = Instant::now();
            let (scheduled, wait) = schedule(map.get(key).copied(), now);
            let jitter = self.jitter(); // sync; rng dropped before any await
            let scheduled = scheduled + jitter;
            map.insert(key.to_string(), scheduled + self.interval);
            wait + jitter
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }

    fn jitter(&self) -> Duration {
        if self.jitter_frac <= 0.0 {
            return Duration::ZERO;
        }
        let f: f64 = rand::thread_rng().gen::<f64>() * self.jitter_frac;
        self.interval.mul_f64(f)
    }
}

/// Pure scheduling math (no clock, no I/O): given the earliest-next-allowed time
/// for a key and `now`, return `(scheduled_at, wait)` ignoring jitter. A request
/// goes at `max(now, earliest)`, waiting the difference.
pub fn schedule(earliest_next: Option<Instant>, now: Instant) -> (Instant, Duration) {
    let scheduled = earliest_next.map(|e| e.max(now)).unwrap_or(now);
    (scheduled, scheduled.saturating_duration_since(now))
}

/// Exponential backoff for a retry `attempt` (0-based): `base · 2^attempt`,
/// capped. Pure — the schedule is unit-testable without sleeping.
pub fn backoff_delay(attempt: u32, base: Duration, cap: Duration) -> Duration {
    let scaled = base.saturating_mul(1u32 << attempt.min(16));
    scaled.min(cap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_prior_request_waits_zero() {
        let now = Instant::now();
        let (_, wait) = schedule(None, now);
        assert_eq!(wait, Duration::ZERO);
    }

    #[test]
    fn a_pending_slot_in_the_future_waits_for_it() {
        let now = Instant::now();
        let earliest = now + Duration::from_millis(200);
        let (sched, wait) = schedule(Some(earliest), now);
        assert_eq!(sched, earliest);
        assert!((wait.as_millis() as i64 - 200).abs() <= 1, "waits ~200ms, got {wait:?}");
    }

    #[test]
    fn an_elapsed_slot_waits_zero() {
        let now = Instant::now();
        let earliest = now - Duration::from_millis(50); // already passed
        let (_, wait) = schedule(Some(earliest), now);
        assert_eq!(wait, Duration::ZERO);
    }

    #[test]
    fn backoff_grows_then_caps() {
        let base = Duration::from_secs(2);
        let cap = Duration::from_secs(60);
        assert_eq!(backoff_delay(0, base, cap), Duration::from_secs(2));
        assert_eq!(backoff_delay(1, base, cap), Duration::from_secs(4));
        assert_eq!(backoff_delay(3, base, cap), Duration::from_secs(16));
        assert_eq!(backoff_delay(10, base, cap), cap, "must cap, not overflow");
    }

    #[tokio::test]
    async fn requests_to_one_domain_are_spaced_at_least_the_interval() {
        // 20 req/s → 50ms interval, no jitter for an exact lower-bound assertion.
        let lim = DomainLimiter::new(20.0, 0.0);
        let mut stamps = Vec::new();
        for _ in 0..4 {
            lim.throttle("example.com").await;
            stamps.push(Instant::now());
        }
        for w in stamps.windows(2) {
            let gap = w[1].duration_since(w[0]);
            assert!(gap >= Duration::from_millis(45), "gap {gap:?} below the 50ms interval");
        }
    }

    #[tokio::test]
    async fn additive_jitter_never_drops_below_the_interval() {
        let lim = DomainLimiter::new(20.0, 0.5); // +0..50% jitter
        let mut stamps = Vec::new();
        for _ in 0..4 {
            lim.throttle("example.com").await;
            stamps.push(Instant::now());
        }
        for w in stamps.windows(2) {
            let gap = w[1].duration_since(w[0]);
            assert!(gap >= Duration::from_millis(45), "jitter must add, never subtract: {gap:?}");
        }
    }

    #[tokio::test]
    async fn different_domains_do_not_block_each_other() {
        let lim = DomainLimiter::new(2.0, 0.0); // 500ms interval
        let start = Instant::now();
        lim.throttle("a.example").await;
        lim.throttle("b.example").await; // different key → no wait
        assert!(start.elapsed() < Duration::from_millis(100), "distinct domains are independent");
    }
}
