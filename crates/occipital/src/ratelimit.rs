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
//!
//! Buckets are keyed by **registrable domain** (Phase 16), so hopping between
//! `www.`/`api.`/`cdn.` subdomains shares one site budget instead of opening a
//! fresh one per hostname. A site's `Crawl-delay` can *raise* its bucket's
//! interval (never lower it) via [`DomainLimiter::set_min_interval`].

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rand::Rng;

pub struct DomainLimiter {
    interval:    Duration,
    jitter_frac: f64,
    /// domain key → earliest `Instant` the next request to it may be scheduled.
    next_allowed: Mutex<HashMap<String, Instant>>,
    /// domain key → a *raised* interval (robots `Crawl-delay`).
    overrides:    Mutex<HashMap<String, Duration>>,
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
            overrides: Mutex::new(HashMap::new()),
        }
    }

    /// Raise `key`'s minimum interval (a site's `Crawl-delay`). Politeness only
    /// ratchets **up**: a shorter request than our configured pace is ignored,
    /// and an existing longer override is kept. Returns `true` if it applied.
    pub fn set_min_interval(&self, key: &str, interval: Duration) -> bool {
        let previous = self.interval_for(key);
        if interval <= previous {
            return false;
        }
        self.overrides.lock().unwrap().insert(key.to_string(), interval);
        // A slot already reserved under the *shorter* interval would let the
        // very next request land before the site's requested spacing — push it
        // out by the difference. (Learning a Crawl-delay from robots.txt always
        // happens right after a request to that same domain.)
        let mut map = self.next_allowed.lock().unwrap();
        if let Some(next) = map.get(key).copied() {
            map.insert(key.to_string(), next + (interval - previous));
        }
        true
    }

    /// The effective interval for a key: the configured pace, or a larger
    /// site-requested one.
    fn interval_for(&self, key: &str) -> Duration {
        let base = self.interval;
        match self.overrides.lock().unwrap().get(key) {
            Some(d) if *d > base => *d,
            _ => base,
        }
    }

    /// Block until it is polite to hit `key` again, reserving this request's
    /// slot. Returns how long the caller waited (for the request log).
    pub async fn throttle(&self, key: &str) -> Duration {
        let interval = self.interval_for(key);
        if interval.is_zero() {
            return Duration::ZERO;
        }
        let wait = {
            // Short, await-free critical section (std Mutex is correct here).
            let mut map = self.next_allowed.lock().unwrap();
            let now = Instant::now();
            let (scheduled, wait) = schedule(map.get(key).copied(), now);
            let jitter = self.jitter(interval); // sync; rng dropped before any await
            let scheduled = scheduled + jitter;
            map.insert(key.to_string(), scheduled + interval);
            wait + jitter
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        wait
    }

    fn jitter(&self, interval: Duration) -> Duration {
        if self.jitter_frac <= 0.0 {
            return Duration::ZERO;
        }
        let f: f64 = rand::thread_rng().gen::<f64>() * self.jitter_frac;
        interval.mul_f64(f)
    }
}

/// Multi-label public suffixes we recognize when deriving a registrable domain.
///
/// This is a curated list, not the full Public Suffix List — deliberately: the
/// PSL is ~10k entries of build weight for a Nano-first binary, and **every
/// error here lands on the polite side**. An unknown multi-label suffix makes
/// the bucket *wider* (more requests share one budget → we go slower); it can
/// never split one site into several buckets and hammer it. Shared-hosting
/// suffixes (`github.io`, `pages.dev`, …) are listed so distinct tenants get
/// their own budget rather than queueing behind each other.
const MULTI_LABEL_SUFFIXES: &[&str] = &[
    // ccTLD second levels
    "co.uk", "org.uk", "ac.uk", "gov.uk", "me.uk", "net.uk", "sch.uk",
    "com.au", "net.au", "org.au", "edu.au", "gov.au", "id.au",
    "co.nz", "net.nz", "org.nz", "govt.nz",
    "co.jp", "or.jp", "ne.jp", "ac.jp", "go.jp",
    "co.kr", "or.kr", "co.za", "org.za", "co.il", "co.th", "in.th",
    "co.id", "or.id", "co.in", "net.in", "org.in", "gov.in", "ac.in",
    "com.br", "net.br", "org.br", "gov.br", "com.ar", "com.mx", "com.co",
    "com.pe", "com.cn", "net.cn", "org.cn", "gov.cn", "edu.cn",
    "com.hk", "com.sg", "com.my", "com.ph", "com.vn", "com.tw", "com.tr",
    "com.pk", "com.ua", "com.pl", "com.ru", "com.es", "com.pt", "com.gr",
    // shared hosting / platform suffixes (distinct tenants = distinct budgets)
    "github.io", "gitlab.io", "pages.dev", "workers.dev", "vercel.app",
    "netlify.app", "herokuapp.com", "appspot.com", "azurewebsites.net",
    "blogspot.com", "wordpress.com", "readthedocs.io", "sourceforge.net",
];

/// The rate-limit bucket key for a host: its registrable domain (eTLD+1), so
/// `www.example.com` and `api.example.com` share one site budget. IP literals
/// bucket as themselves.
pub fn registrable_domain(host: &str) -> String {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() || host.starts_with('[') || host.parse::<std::net::IpAddr>().is_ok() {
        return host;
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() <= 2 {
        return host;
    }
    let n = labels.len();
    let last_two = format!("{}.{}", labels[n - 2], labels[n - 1]);
    if MULTI_LABEL_SUFFIXES.contains(&last_two.as_str()) {
        return format!("{}.{}", labels[n - 3], last_two);
    }
    last_two
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

    #[test]
    fn registrable_domain_groups_subdomains_and_respects_known_suffixes() {
        assert_eq!(registrable_domain("www.example.com"), "example.com");
        assert_eq!(registrable_domain("api.cdn.example.com"), "example.com");
        assert_eq!(registrable_domain("Example.COM."), "example.com", "normalized");
        assert_eq!(registrable_domain("example.com"), "example.com");
        assert_eq!(registrable_domain("localhost"), "localhost");
        // Multi-label suffixes: news.bbc.co.uk must NOT collapse to "co.uk".
        assert_eq!(registrable_domain("news.bbc.co.uk"), "bbc.co.uk");
        assert_eq!(registrable_domain("bbc.co.uk"), "bbc.co.uk");
        // Shared hosting: distinct tenants get distinct budgets.
        assert_eq!(registrable_domain("someone.github.io"), "someone.github.io");
        // IP literals bucket as themselves.
        assert_eq!(registrable_domain("127.0.0.1"), "127.0.0.1");
        assert_eq!(registrable_domain("[::1]"), "[::1]");
    }

    #[tokio::test]
    async fn crawl_delay_raises_the_interval_but_never_lowers_it() {
        // Configured pace: 20 req/s (50ms). A site asking for 200ms wins.
        let lim = DomainLimiter::new(20.0, 0.0);
        assert!(lim.set_min_interval("slow.test", Duration::from_millis(200)));
        assert!(!lim.set_min_interval("slow.test", Duration::from_millis(100)),
                "a shorter request never loosens an existing override");
        assert!(!lim.set_min_interval("fast.test", Duration::from_millis(10)),
                "a site asking to be hit faster than our pace is ignored");

        let start = Instant::now();
        lim.throttle("slow.test").await;
        lim.throttle("slow.test").await;
        assert!(start.elapsed() >= Duration::from_millis(190), "crawl-delay honored: {:?}", start.elapsed());

        // The real sequence: a request goes out (reserving a slot at the old
        // pace), *then* robots.txt reveals the Crawl-delay. The already-reserved
        // slot must be pushed out, or the next request lands too early.
        let lim = DomainLimiter::new(20.0, 0.0); // 50ms
        lim.throttle("late.test").await; // slot reserved at +50ms
        assert!(lim.set_min_interval("late.test", Duration::from_millis(300)));
        let start = Instant::now();
        lim.throttle("late.test").await;
        assert!(
            start.elapsed() >= Duration::from_millis(280),
            "a slot reserved before the delay was known must be pushed out, waited {:?}",
            start.elapsed()
        );

        let start = Instant::now();
        lim.throttle("fast.test").await;
        lim.throttle("fast.test").await;
        assert!(start.elapsed() < Duration::from_millis(150), "unaffected domain keeps our pace");
    }

    #[tokio::test]
    async fn throttle_reports_the_wait_it_imposed() {
        let lim = DomainLimiter::new(10.0, 0.0); // 100ms
        assert_eq!(lim.throttle("x.test").await, Duration::ZERO, "first request is free");
        let waited = lim.throttle("x.test").await;
        assert!(waited >= Duration::from_millis(90), "second waited ~100ms, reported {waited:?}");
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
