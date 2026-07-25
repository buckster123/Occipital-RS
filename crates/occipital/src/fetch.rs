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
    pub final_url:     String,
    pub status:        u16,
    pub content_type:  Option<String>,
    /// Validators captured from the response, for a later conditional refresh.
    pub etag:          Option<String>,
    pub last_modified: Option<String>,
    pub body:          Vec<u8>,
    pub source:        Source,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

/// A general request — the building block keyed search providers use (custom
/// headers, POST bodies). `robots` gates only arbitrary page reads, not
/// deliberate provider API calls.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method:  Method,
    pub url:     String,
    pub headers: Vec<(String, String)>,
    pub body:    Option<Vec<u8>>,
    pub robots:  bool,
}

impl HttpRequest {
    /// A no-robots GET with headers (Brave/Bing-style keyed APIs).
    pub fn get_api(url: impl Into<String>, headers: Vec<(String, String)>) -> Self {
        Self { method: Method::Get, url: url.into(), headers, body: None, robots: false }
    }
    /// A no-robots POST with a JSON body (Tavily-style keyed APIs).
    pub fn post_json(url: impl Into<String>, body: impl Into<Vec<u8>>) -> Self {
        Self {
            method: Method::Post,
            url: url.into(),
            headers: vec![("content-type".into(), "application/json".into())],
            body: Some(body.into()),
            robots: false,
        }
    }
    /// A **robots-gated** urlencoded POST — a form submission (`web_submit`).
    /// Unlike provider API calls, this targets an arbitrary site, so the full
    /// politeness contract applies.
    pub fn post_form(url: impl Into<String>, body: impl Into<Vec<u8>>) -> Self {
        Self {
            method: Method::Post,
            url: url.into(),
            headers: vec![("content-type".into(), "application/x-www-form-urlencoded".into())],
            body: Some(body.into()),
            robots: true,
        }
    }
}

/// The fetch seam. Mock it to test everything above the network.
#[async_trait]
pub trait Fetcher: Send + Sync {
    /// Fetch a URL, honoring robots.txt (the path for arbitrary page reads).
    async fn get(&self, url: &str) -> anyhow::Result<FetchResponse>;

    /// Fetch without the robots gate — for *deliberate* API calls to a search
    /// provider (querying a service we mean to use, not crawling its content).
    /// Still throttled + concurrency-capped. Defaults to [`get`](Self::get) so
    /// mocks need not override it.
    async fn get_unchecked(&self, url: &str) -> anyhow::Result<FetchResponse> {
        self.get(url).await
    }

    /// Conditional GET (robots-gated) for a cache refresh: sends `If-None-Match`
    /// / `If-Modified-Since`. A `304` returns with `status == 304` and an empty
    /// body. Defaults to a plain [`get`](Self::get) (always `200`) so a mock
    /// without 304 support simply re-fetches — correct, just unoptimized.
    async fn get_conditional(
        &self,
        url: &str,
        _etag: Option<&str>,
        _last_modified: Option<&str>,
    ) -> anyhow::Result<FetchResponse> {
        self.get(url).await
    }

    /// A general request (custom method/headers/body) — keyed search providers.
    /// Defaults to GET-only via [`get`](Self::get)/[`get_unchecked`](Self::get_unchecked)
    /// (headers dropped, POST unsupported) so simple mocks need not override it.
    async fn request(&self, req: HttpRequest) -> anyhow::Result<FetchResponse> {
        match req.method {
            Method::Get if req.robots => self.get(&req.url).await,
            Method::Get => self.get_unchecked(&req.url).await,
            Method::Post => anyhow::bail!("this fetcher does not support POST"),
        }
    }
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

impl PoliteFetcher {
    fn parse_http(url: &str) -> anyhow::Result<Url> {
        let parsed = Url::parse(url)?;
        if !matches!(parsed.scheme(), "http" | "https") {
            anyhow::bail!("unsupported scheme: {}", parsed.scheme());
        }
        Ok(parsed)
    }

    /// Throttle → concurrency permit → send (method/headers/body) with polite
    /// backoff on 429/503. The robots check is the caller's responsibility (so
    /// provider API calls skip it). A `304` is returned as-is (empty body) — not
    /// retried. **Only GET is ever retried**: a POST is not idempotent (it may
    /// have reached the server), so a 429/503/transport error on one returns
    /// honestly instead of replaying the write (docs/agent-browsing.md).
    async fn send_core(
        &self,
        parsed: Url,
        method: Method,
        headers: Vec<(String, String)>,
        body: Option<Vec<u8>>,
    ) -> anyhow::Result<FetchResponse> {
        self.limiter.throttle(&Self::rate_key(&parsed)).await; // no permit held while waiting
        let _permit = self.sem.acquire().await?;
        let retriable = method_retriable(method);
        let mut attempt = 0u32;
        loop {
            let mut req = match method {
                Method::Get => self.client.get(parsed.clone()),
                Method::Post => self.client.post(parsed.clone()),
            };
            for (k, v) in &headers {
                req = req.header(k.as_str(), v.as_str());
            }
            if let Some(b) = &body {
                req = req.body(b.clone());
            }
            match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if retriable && (status.as_u16() == 429 || status.as_u16() == 503) && attempt < self.max_retries {
                        let wait = retry_after(&resp)
                            .unwrap_or_else(|| backoff_delay(attempt, BACKOFF_BASE, BACKOFF_CAP));
                        attempt += 1;
                        tracing::debug!(url = %parsed, ?wait, status = status.as_u16(), "backing off");
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    if let Some(len) = resp.content_length() {
                        if len as usize > self.max_body_bytes {
                            anyhow::bail!("body too large: {len} bytes > cap {}", self.max_body_bytes);
                        }
                    }
                    let header = |h: reqwest::header::HeaderName| {
                        resp.headers().get(h).and_then(|v| v.to_str().ok()).map(String::from)
                    };
                    let final_url = resp.url().to_string();
                    let content_type = header(reqwest::header::CONTENT_TYPE);
                    let etag = header(reqwest::header::ETAG);
                    let last_modified = header(reqwest::header::LAST_MODIFIED);
                    let status_u16 = status.as_u16();
                    let mut body = resp.bytes().await?.to_vec();
                    body.truncate(self.max_body_bytes); // backstop for chunked/no-length
                    return Ok(FetchResponse {
                        final_url,
                        status: status_u16,
                        content_type,
                        etag,
                        last_modified,
                        body,
                        source: Source::Network,
                    });
                }
                Err(e) => {
                    if retriable && attempt < self.max_retries && (e.is_timeout() || e.is_connect()) {
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

    async fn robots_gate(&self, parsed: &Url, url: &str) -> anyhow::Result<()> {
        if !self.respect_robots {
            return Ok(());
        }
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
        Ok(())
    }
}

#[async_trait]
impl Fetcher for PoliteFetcher {
    async fn get(&self, url: &str) -> anyhow::Result<FetchResponse> {
        let parsed = Self::parse_http(url)?;
        self.robots_gate(&parsed, url).await?;
        self.send_core(parsed, Method::Get, Vec::new(), None).await
    }

    async fn get_unchecked(&self, url: &str) -> anyhow::Result<FetchResponse> {
        let parsed = Self::parse_http(url)?;
        self.send_core(parsed, Method::Get, Vec::new(), None).await
    }

    async fn get_conditional(
        &self,
        url: &str,
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> anyhow::Result<FetchResponse> {
        let parsed = Self::parse_http(url)?;
        self.robots_gate(&parsed, url).await?;
        let mut headers = Vec::new();
        if let Some(e) = etag {
            headers.push(("if-none-match".to_string(), e.to_string()));
        }
        if let Some(lm) = last_modified {
            headers.push(("if-modified-since".to_string(), lm.to_string()));
        }
        self.send_core(parsed, Method::Get, headers, None).await
    }

    async fn request(&self, req: HttpRequest) -> anyhow::Result<FetchResponse> {
        let parsed = Self::parse_http(&req.url)?;
        if req.robots {
            self.robots_gate(&parsed, &req.url).await?;
        }
        self.send_core(parsed, req.method, req.headers, req.body).await
    }
}

/// Whether a method may be transparently retried after 429/503/transport
/// errors. Only GET: it is idempotent by contract; a POST may already have
/// acted on the server, so replaying it is the fetcher's call to refuse.
fn method_retriable(method: Method) -> bool {
    matches!(method, Method::Get)
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
    fn only_get_is_retriable() {
        assert!(method_retriable(Method::Get));
        assert!(!method_retriable(Method::Post), "a POST is never replayed");
    }

    #[test]
    fn post_form_is_urlencoded_and_robots_gated() {
        let r = HttpRequest::post_form("https://e.test/contact", b"q=hi".to_vec());
        assert_eq!(r.method, Method::Post);
        assert!(r.robots, "form submissions honor robots.txt");
        assert_eq!(r.headers[0].1, "application/x-www-form-urlencoded");
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
