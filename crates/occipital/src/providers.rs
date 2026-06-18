//! Search providers — pluggable behind the [`SearchProvider`] trait.
//!
//! Phase 3 ships the two zero-key defaults: `duckduckgo` (HTML scrape of the
//! lite endpoint) and `searxng` (JSON API of a self-hosted/instance). Keyed
//! providers (Brave/Tavily/Bing) land in Phase 7. Every provider issues its
//! request through the polite [`Fetcher`] (rate-limited) and **fails soft** —
//! markup/JSON drift yields empty results + a log line, never a panic.

use async_trait::async_trait;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::config::Config;
use crate::fetch::{Fetcher, HttpRequest};
use crate::keys::Keys;

/// One ranked search hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResult {
    pub title:   String,
    pub url:     String,
    pub snippet: String,
    pub rank:    usize,
}

#[async_trait]
pub trait SearchProvider: Send + Sync {
    fn name(&self) -> &str;
    async fn search(
        &self,
        fetcher: &dyn Fetcher,
        query: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<SearchResult>>;
}

/// Select the provider for a config + key store. A keyed provider is used only
/// when its key is present; otherwise (and for unknown providers) we fall back
/// to DuckDuckGo with a warning, so a node is never stuck without results.
pub fn provider_for(cfg: &Config, keys: &Keys) -> Box<dyn SearchProvider> {
    let name = cfg.search_provider.to_ascii_lowercase();
    match name.as_str() {
        "duckduckgo" | "ddg" => Box::new(DuckDuckGo),
        "searxng" => match &cfg.searxng_url {
            Some(base) => Box::new(SearxNG { base: base.clone() }),
            None => {
                tracing::warn!("searxng selected but OCCIPITAL_SEARXNG_URL unset — using duckduckgo");
                Box::new(DuckDuckGo)
            }
        },
        "brave" | "tavily" | "bing" => match keys.get(&name) {
            Some(key) => keyed_provider(&name, key),
            None => {
                tracing::warn!("'{name}' selected but no API key set — using duckduckgo (set OCCIPITAL_{}_KEY)", name.to_ascii_uppercase());
                Box::new(DuckDuckGo)
            }
        },
        other => {
            tracing::warn!("unknown search provider '{other}' — using duckduckgo");
            Box::new(DuckDuckGo)
        }
    }
}

fn keyed_provider(name: &str, key: String) -> Box<dyn SearchProvider> {
    match name {
        "brave" => Box::new(Brave { key }),
        "tavily" => Box::new(Tavily { key }),
        "bing" => Box::new(Bing { key }),
        _ => Box::new(DuckDuckGo),
    }
}

// --------------------------------------------------------------------------
// DuckDuckGo — HTML scrape of the lite endpoint (no key, no JS)
// --------------------------------------------------------------------------

pub struct DuckDuckGo;

#[async_trait]
impl SearchProvider for DuckDuckGo {
    fn name(&self) -> &str {
        "duckduckgo"
    }

    async fn search(
        &self,
        fetcher: &dyn Fetcher,
        query: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<SearchResult>> {
        let url = Url::parse_with_params("https://html.duckduckgo.com/html/", &[("q", query)])?;
        let resp = fetcher.get_unchecked(url.as_str()).await?;
        let html = String::from_utf8_lossy(&resp.body);
        Ok(parse_ddg(&html, limit))
    }
}

fn parse_ddg(html: &str, limit: usize) -> Vec<SearchResult> {
    let doc = Html::parse_document(html);
    let result_sel = Selector::parse("div.result").expect("static selector");
    let link_sel = Selector::parse("a.result__a").expect("static selector");
    let snippet_sel = Selector::parse(".result__snippet").expect("static selector");

    let mut out = Vec::new();
    for res in doc.select(&result_sel) {
        if out.len() >= limit {
            break;
        }
        let Some(a) = res.select(&link_sel).next() else { continue };
        let title = a.text().collect::<String>().trim().to_string();
        let Some(real) = ddg_real_url(a.value().attr("href").unwrap_or("")) else { continue };
        let snippet = res
            .select(&snippet_sel)
            .next()
            .map(|s| s.text().collect::<String>().trim().to_string())
            .unwrap_or_default();
        let rank = out.len();
        out.push(SearchResult { title, url: real, snippet, rank });
    }
    if out.is_empty() {
        tracing::debug!("duckduckgo: no results parsed (markup drift or empty query?)");
    }
    out
}

/// DuckDuckGo wraps each result URL in a `/l/?uddg=<encoded>` redirect; unwrap
/// it to the real destination. A direct http(s) href is taken as-is.
fn ddg_real_url(href: &str) -> Option<String> {
    let resolved = Url::parse("https://html.duckduckgo.com/").ok()?.join(href).ok()?;
    if let Some((_, v)) = resolved.query_pairs().find(|(k, _)| k == "uddg") {
        return Some(v.into_owned()); // query_pairs already percent-decodes
    }
    let is_redirect = resolved.path().contains("/l/");
    (matches!(resolved.scheme(), "http" | "https") && resolved.host().is_some() && !is_redirect)
        .then(|| resolved.to_string())
}

// --------------------------------------------------------------------------
// SearXNG — JSON API of a self-hosted / instance
// --------------------------------------------------------------------------

pub struct SearxNG {
    base: String,
}

#[async_trait]
impl SearchProvider for SearxNG {
    fn name(&self) -> &str {
        "searxng"
    }

    async fn search(
        &self,
        fetcher: &dyn Fetcher,
        query: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<SearchResult>> {
        let endpoint = format!("{}/search", self.base.trim_end_matches('/'));
        let url = Url::parse_with_params(&endpoint, &[("q", query), ("format", "json")])?;
        let resp = fetcher.get_unchecked(url.as_str()).await?;
        let json: Value = serde_json::from_slice(&resp.body)?;
        Ok(parse_searxng(&json, limit))
    }
}

fn parse_searxng(json: &Value, limit: usize) -> Vec<SearchResult> {
    json["results"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    let url = r["url"].as_str()?.to_string();
                    Some(SearchResult {
                        title:   r["title"].as_str().unwrap_or("").to_string(),
                        url,
                        snippet: r["content"].as_str().unwrap_or("").to_string(),
                        rank:    0, // filled below
                    })
                })
                .take(limit)
                .enumerate()
                .map(|(i, mut r)| {
                    r.rank = i;
                    r
                })
                .collect()
        })
        .unwrap_or_default()
}

// --------------------------------------------------------------------------
// Keyed providers — paid APIs (programmatic by design), queried through the
// Fetcher::request seam (custom headers / POST). Each fails soft on drift.
// --------------------------------------------------------------------------

/// Brave Search API — GET + `X-Subscription-Token`.
pub struct Brave {
    key: String,
}

#[async_trait]
impl SearchProvider for Brave {
    fn name(&self) -> &str {
        "brave"
    }
    async fn search(&self, fetcher: &dyn Fetcher, query: &str, limit: usize) -> anyhow::Result<Vec<SearchResult>> {
        let url = Url::parse_with_params(
            "https://api.search.brave.com/res/v1/web/search",
            &[("q", query), ("count", &limit.to_string())],
        )?;
        let req = HttpRequest::get_api(
            url.to_string(),
            vec![
                ("accept".into(), "application/json".into()),
                ("x-subscription-token".into(), self.key.clone()),
            ],
        );
        let resp = fetcher.request(req).await?;
        Ok(parse_brave(&serde_json::from_slice(&resp.body)?, limit))
    }
}

fn parse_brave(json: &Value, limit: usize) -> Vec<SearchResult> {
    json["web"]["results"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    Some(SearchResult {
                        title:   r["title"].as_str().unwrap_or("").to_string(),
                        url:     r["url"].as_str()?.to_string(),
                        snippet: r["description"].as_str().unwrap_or("").to_string(),
                        rank:    0,
                    })
                })
                .take(limit)
                .enumerate()
                .map(|(i, mut r)| { r.rank = i; r })
                .collect()
        })
        .unwrap_or_default()
}

/// Tavily — POST JSON (key in the body), built for LLM agents.
pub struct Tavily {
    key: String,
}

#[async_trait]
impl SearchProvider for Tavily {
    fn name(&self) -> &str {
        "tavily"
    }
    async fn search(&self, fetcher: &dyn Fetcher, query: &str, limit: usize) -> anyhow::Result<Vec<SearchResult>> {
        let body = serde_json::json!({
            "api_key": self.key,
            "query": query,
            "max_results": limit,
        })
        .to_string();
        let resp = fetcher.request(HttpRequest::post_json("https://api.tavily.com/search", body.into_bytes())).await?;
        Ok(parse_tavily(&serde_json::from_slice(&resp.body)?, limit))
    }
}

fn parse_tavily(json: &Value, limit: usize) -> Vec<SearchResult> {
    json["results"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    Some(SearchResult {
                        title:   r["title"].as_str().unwrap_or("").to_string(),
                        url:     r["url"].as_str()?.to_string(),
                        snippet: r["content"].as_str().unwrap_or("").to_string(),
                        rank:    0,
                    })
                })
                .take(limit)
                .enumerate()
                .map(|(i, mut r)| { r.rank = i; r })
                .collect()
        })
        .unwrap_or_default()
}

/// Bing Web Search API — GET + `Ocp-Apim-Subscription-Key`.
pub struct Bing {
    key: String,
}

#[async_trait]
impl SearchProvider for Bing {
    fn name(&self) -> &str {
        "bing"
    }
    async fn search(&self, fetcher: &dyn Fetcher, query: &str, limit: usize) -> anyhow::Result<Vec<SearchResult>> {
        let url = Url::parse_with_params(
            "https://api.bing.microsoft.com/v7.0/search",
            &[("q", query), ("count", &limit.to_string())],
        )?;
        let req = HttpRequest::get_api(
            url.to_string(),
            vec![("ocp-apim-subscription-key".into(), self.key.clone())],
        );
        let resp = fetcher.request(req).await?;
        Ok(parse_bing(&serde_json::from_slice(&resp.body)?, limit))
    }
}

fn parse_bing(json: &Value, limit: usize) -> Vec<SearchResult> {
    json["webPages"]["value"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    Some(SearchResult {
                        title:   r["name"].as_str().unwrap_or("").to_string(),
                        url:     r["url"].as_str()?.to_string(),
                        snippet: r["snippet"].as_str().unwrap_or("").to_string(),
                        rank:    0,
                    })
                })
                .take(limit)
                .enumerate()
                .map(|(i, mut r)| { r.rank = i; r })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    // A minimal fixture mirroring the html.duckduckgo.com result structure.
    const DDG_HTML: &str = r#"
      <div class="result results_links web-result">
        <div class="links_main">
          <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Frust&rut=abc">The Rust Language</a>
          <a class="result__snippet" href="...">A language empowering everyone.</a>
        </div>
      </div>
      <div class="result results_links web-result">
        <div class="links_main">
          <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdoc.rust-lang.org%2Fbook%2F">The Book</a>
          <a class="result__snippet">Learn Rust.</a>
        </div>
      </div>
    "#;

    #[test]
    fn ddg_parses_results_and_unwraps_redirect_urls() {
        let results = parse_ddg(DDG_HTML, 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "The Rust Language");
        assert_eq!(results[0].url, "https://example.com/rust", "uddg decoded to the real URL");
        assert_eq!(results[0].snippet, "A language empowering everyone.");
        assert_eq!(results[0].rank, 0);
        assert_eq!(results[1].url, "https://doc.rust-lang.org/book/");
        assert_eq!(results[1].rank, 1);
    }

    #[test]
    fn ddg_respects_the_limit() {
        assert_eq!(parse_ddg(DDG_HTML, 1).len(), 1);
    }

    #[test]
    fn ddg_empty_markup_yields_no_results_not_a_panic() {
        assert!(parse_ddg("<html><body>nope</body></html>", 5).is_empty());
    }

    #[test]
    fn searxng_parses_json_results() {
        let json: Value = serde_json::json!({
            "results": [
                {"url": "https://a.test/1", "title": "A", "content": "snippet a"},
                {"url": "https://b.test/2", "title": "B", "content": "snippet b"},
                {"title": "no url — skipped", "content": "x"}
            ]
        });
        let results = parse_searxng(&json, 10);
        assert_eq!(results.len(), 2, "result without a url is dropped");
        assert_eq!(results[0].url, "https://a.test/1");
        assert_eq!(results[0].title, "A");
        assert_eq!(results[1].rank, 1);
    }

    #[test]
    fn searxng_missing_results_array_is_empty() {
        assert!(parse_searxng(&serde_json::json!({"error": "boom"}), 5).is_empty());
    }

    // ---- keyed providers ------------------------------------------------

    use crate::fetch::{FetchResponse, Source};
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// Records the request it received and returns a canned body.
    struct Recorder {
        body: Vec<u8>,
        last: Mutex<Option<HttpRequest>>,
    }
    #[async_trait]
    impl Fetcher for Recorder {
        async fn get(&self, url: &str) -> anyhow::Result<FetchResponse> {
            Ok(FetchResponse {
                final_url: url.into(), status: 200, content_type: None,
                etag: None, last_modified: None, body: self.body.clone(), source: Source::Network,
            })
        }
        async fn request(&self, req: HttpRequest) -> anyhow::Result<FetchResponse> {
            let url = req.url.clone();
            *self.last.lock().unwrap() = Some(req);
            self.get(&url).await
        }
    }

    #[tokio::test]
    async fn brave_sends_key_header_bypasses_robots_and_parses() {
        let body = br#"{"web":{"results":[{"title":"Rust","url":"https://rust-lang.org","description":"d"}]}}"#.to_vec();
        let rec = Recorder { body, last: Mutex::new(None) };
        let results = Brave { key: "K".into() }.search(&rec, "rust", 3).await.unwrap();
        assert_eq!(results[0].url, "https://rust-lang.org");
        let req = rec.last.lock().unwrap().clone().unwrap();
        assert!(req.headers.iter().any(|(k, v)| k == "x-subscription-token" && v == "K"), "sends the key header");
        assert!(req.url.contains("api.search.brave.com"));
        assert!(!req.robots, "a provider API call is not robots-gated");
    }

    #[tokio::test]
    async fn tavily_posts_json_with_the_key() {
        let body = br#"{"results":[{"title":"T","url":"https://t.test","content":"c"}]}"#.to_vec();
        let rec = Recorder { body, last: Mutex::new(None) };
        let results = Tavily { key: "SECRET".into() }.search(&rec, "q", 2).await.unwrap();
        assert_eq!(results[0].url, "https://t.test");
        let req = rec.last.lock().unwrap().clone().unwrap();
        assert_eq!(req.method, crate::fetch::Method::Post);
        let sent = String::from_utf8(req.body.unwrap()).unwrap();
        assert!(sent.contains("SECRET") && sent.contains("\"query\":\"q\""), "key+query in body: {sent}");
    }

    #[test]
    fn parse_bing_extracts_results() {
        let json = serde_json::json!({"webPages":{"value":[
            {"name":"B","url":"https://b.test","snippet":"s"}
        ]}});
        let r = parse_bing(&json, 5);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].title, "B");
        assert_eq!(r[0].url, "https://b.test");
    }

    #[test]
    fn keyed_parsers_fail_soft_on_drift() {
        assert!(parse_brave(&serde_json::json!({"unexpected": true}), 5).is_empty());
        assert!(parse_tavily(&serde_json::json!({}), 5).is_empty());
        assert!(parse_bing(&serde_json::json!({"webPages": {}}), 5).is_empty());
    }

    #[test]
    fn provider_for_uses_keyed_when_key_present_else_falls_back() {
        let mut cfg = Config::from_env().unwrap();
        cfg.search_provider = "brave".into();
        let dir = tempfile::TempDir::new().unwrap();
        let mut keys = Keys::load(&dir.path().join("k.json"));
        assert_eq!(provider_for(&cfg, &keys).name(), "duckduckgo", "no key → graceful fallback");
        keys.set("brave", "K");
        assert_eq!(provider_for(&cfg, &keys).name(), "brave", "key present → keyed provider");
    }
}
