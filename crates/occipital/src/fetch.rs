//! The polite HTTP layer — the only thing in Occipital that touches the network.
//!
//! Everything above it goes through the [`Fetcher`] trait, so extraction, the
//! cache, and the providers are all testable against a mock with no network. The
//! real [`PoliteFetcher`] enforces the politeness contract (docs/politeness.md):
//! robots gate → per-domain throttle → global concurrency cap → polite backoff.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{Mutex, Semaphore};
use url::Url;

use crate::config::Config;
use crate::ratelimit::{backoff_delay, DomainLimiter};
use crate::robots::Robots;

/// Where a response came from (the cache layer adds `Cache` in a later phase).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Network,
}

#[derive(Debug, Clone)]
pub struct FetchResponse {
    pub final_url:    String,
    pub status:       u16,
    pub content_type: Option<String>,
    pub body:         Vec<u8>,
    pub source:       Source,
}

/// The fetch seam. Mock it to test everything above the network.
#[async_trait]
pub trait Fetcher: Send + Sync {
    async fn get(&self, url: &str) -> anyhow::Result<FetchResponse>;
}

const BACKOFF_BASE: Duration = Duration::from_secs(2);
const BACKOFF_CAP: Duration = Duration::from_secs(60);
const JITTER_FRAC: f64 = 0.3;

pub struct PoliteFetcher {
    client:         reqwest::Client,
    limiter:        DomainLimiter,
    sem:            Semaphore,
    robots:         Mutex<HashMap<String, Arc<Robots>>>,
    respect_robots: bool,
    ua_token:       String,
    max_body_bytes: usize,
    max_retries:    u32,
}

impl PoliteFetcher {
    pub fn new(cfg: &Config) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(cfg.user_agent.clone())
            .timeout(Duration::from_secs(cfg.fetch_timeout_secs))
            .gzip(true)
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()?;
        Ok(Self {
            client,
            limiter:        DomainLimiter::new(cfg.rate_per_domain, JITTER_FRAC),
            sem:            Semaphore::new(cfg.max_concurrency.max(1)),
            robots:         Mutex::new(HashMap::new()),
            respect_robots: cfg.respect_robots,
            ua_token:       ua_token(&cfg.user_agent),
            max_body_bytes: cfg.max_body_bytes,
            max_retries:    cfg.max_retries,
        })
    }

    /// The rate-limit bucket key for a URL. Host-level for now; eTLD+1 grouping
    /// (so sibling subdomains share a budget) is a deliberate later refinement.
    fn rate_key(url: &Url) -> String {
        url.host_str().unwrap_or("").to_ascii_lowercase()
    }

    /// Fetch + cache a host's robots.txt. A missing/unreadable file → allow-all.
    /// The robots.txt request is itself throttled (it's a hit on the domain) but
    /// not robots-gated (no recursion).
    async fn robots_for(&self, scheme: &str, host: &str) -> Arc<Robots> {
        if let Some(r) = self.robots.lock().await.get(host) {
            return r.clone();
        }
        self.limiter.throttle(host).await;
        let robots = {
            let _permit = self.sem.acquire().await.ok();
            let url = format!("{scheme}://{host}/robots.txt");
            match self.client.get(&url).send().await {
                Ok(r) if r.status().is_success() => {
                    let txt = r.text().await.unwrap_or_default();
                    Robots::parse(&txt, &self.ua_token)
                }
                _ => Robots::allow_all(),
            }
        };
        let robots = Arc::new(robots);
        self.robots.lock().await.insert(host.to_string(), robots.clone());
        robots
    }
}

#[async_trait]
impl Fetcher for PoliteFetcher {
    async fn get(&self, url: &str) -> anyhow::Result<FetchResponse> {
        let parsed = Url::parse(url)?;
        if !matches!(parsed.scheme(), "http" | "https") {
            anyhow::bail!("unsupported scheme: {}", parsed.scheme());
        }

        // 1. robots gate
        if self.respect_robots {
            if let Some(host) = parsed.host_str() {
                let robots = self.robots_for(parsed.scheme(), host).await;
                let path_q = match parsed.query() {
                    Some(q) => format!("{}?{}", parsed.path(), q),
                    None => parsed.path().to_string(),
                };
                if !robots.allowed(&path_q) {
                    anyhow::bail!("blocked by robots.txt: {url}");
                }
            }
        }

        // 2. per-domain throttle (no concurrency permit held while merely waiting)
        self.limiter.throttle(&Self::rate_key(&parsed)).await;

        // 3. concurrency cap, then 4. send with polite backoff on 429/503
        let _permit = self.sem.acquire().await?;
        let mut attempt = 0u32;
        loop {
            match self.client.get(parsed.clone()).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if (status.as_u16() == 429 || status.as_u16() == 503) && attempt < self.max_retries {
                        let wait = retry_after(&resp)
                            .unwrap_or_else(|| backoff_delay(attempt, BACKOFF_BASE, BACKOFF_CAP));
                        attempt += 1;
                        tracing::debug!(%url, ?wait, status = status.as_u16(), "backing off");
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    if let Some(len) = resp.content_length() {
                        if len as usize > self.max_body_bytes {
                            anyhow::bail!("body too large: {len} bytes > cap {}", self.max_body_bytes);
                        }
                    }
                    let final_url = resp.url().to_string();
                    let content_type = resp
                        .headers()
                        .get(reqwest::header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string());
                    let mut body = resp.bytes().await?.to_vec();
                    body.truncate(self.max_body_bytes); // backstop for chunked/no-length
                    return Ok(FetchResponse {
                        final_url,
                        status: status.as_u16(),
                        content_type,
                        body,
                        source: Source::Network,
                    });
                }
                Err(e) => {
                    if attempt < self.max_retries && (e.is_timeout() || e.is_connect()) {
                        let wait = backoff_delay(attempt, BACKOFF_BASE, BACKOFF_CAP);
                        attempt += 1;
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    return Err(e.into());
                }
            }
        }
    }
}

/// Extract the product token from a UA string: `"Occipital/0.1 (…)"` → `"occipital"`.
fn ua_token(ua: &str) -> String {
    ua.split('/').next().unwrap_or(ua).trim().to_ascii_lowercase()
}

/// Parse a `Retry-After` header as whole seconds (HTTP-date form falls back to
/// exponential backoff at the call site).
fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ua_token_extracts_the_product() {
        assert_eq!(ua_token("Occipital/0.1 (+https://x; reader)"), "occipital");
        assert_eq!(ua_token("SomeBot"), "somebot");
    }

    #[test]
    fn rate_key_is_host_lowercased() {
        let u = Url::parse("https://Example.COM/a/b?q=1").unwrap();
        assert_eq!(PoliteFetcher::rate_key(&u), "example.com");
    }

    #[test]
    fn builds_from_config_without_panic() {
        let cfg = Config::from_env().unwrap();
        assert!(PoliteFetcher::new(&cfg).is_ok());
    }

    // A real network fetch — run manually with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_fetch_example_com() {
        let cfg = Config::from_env().unwrap();
        let f = PoliteFetcher::new(&cfg).unwrap();
        let r = f.get("https://example.com/").await.unwrap();
        assert_eq!(r.status, 200);
        assert!(!r.body.is_empty());
    }
}
