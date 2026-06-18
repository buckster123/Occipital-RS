//! Search providers — pluggable behind the [`SearchProvider`] trait.
//!
//! Phase 3 ships the two zero-key defaults: `duckduckgo` (HTML scrape of the
//! lite endpoint) and `searxng` (JSON API of a self-hosted/instance). Keyed
//! providers (Brave/Tavily/Bing) land in Phase 7. Every provider issues its
//! request through the polite [`Fetcher`] (rate-limited) and **fails soft** —
//! markup/JSON drift yields empty results + a log line, never a panic.

use async_trait::async_trait;
use scraper::{Html, Selector};
use serde::Serialize;
use serde_json::Value;
use url::Url;

use crate::config::Config;
use crate::fetch::Fetcher;

/// One ranked search hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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

/// Select the provider for a config. Unknown / not-yet-implemented (keyed)
/// providers fall back to DuckDuckGo with a warning, so a node is never stuck.
pub fn provider_for(cfg: &Config) -> Box<dyn SearchProvider> {
    match cfg.search_provider.to_ascii_lowercase().as_str() {
        "duckduckgo" | "ddg" => Box::new(DuckDuckGo),
        "searxng" => match &cfg.searxng_url {
            Some(base) => Box::new(SearxNG { base: base.clone() }),
            None => {
                tracing::warn!("searxng selected but OCCIPITAL_SEARXNG_URL unset — using duckduckgo");
                Box::new(DuckDuckGo)
            }
        },
        other @ ("brave" | "tavily" | "bing") => {
            tracing::warn!("keyed provider '{other}' lands in Phase 7 — using duckduckgo for now");
            Box::new(DuckDuckGo)
        }
        other => {
            tracing::warn!("unknown search provider '{other}' — using duckduckgo");
            Box::new(DuckDuckGo)
        }
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
}
