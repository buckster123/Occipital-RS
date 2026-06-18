//! The Engine — what every binary drives. Owns the polite fetcher and the
//! selected search provider, and exposes the two verbs the tool surface needs:
//! [`search`](Engine::search) (provider results) and [`fetch`](Engine::fetch)
//! (a URL → reader-mode page). The read-through cache wraps these in Phase 4.

use std::sync::Arc;

use crate::config::Config;
use crate::extract::{extract_bytes, Page};
use crate::fetch::{Fetcher, PoliteFetcher};
use crate::providers::{provider_for, SearchProvider, SearchResult};

pub struct Engine {
    fetcher:  Arc<dyn Fetcher>,
    provider: Box<dyn SearchProvider>,
    top_n:    usize,
}

impl Engine {
    /// Build the production engine: a [`PoliteFetcher`] + the config's provider.
    pub fn from_config(cfg: &Config) -> anyhow::Result<Self> {
        let fetcher: Arc<dyn Fetcher> = Arc::new(PoliteFetcher::new(cfg)?);
        Ok(Self { fetcher, provider: provider_for(cfg), top_n: cfg.search_top_n })
    }

    /// Inject a fetcher + provider directly (tests; or a custom embedding).
    pub fn with_parts(fetcher: Arc<dyn Fetcher>, provider: Box<dyn SearchProvider>, top_n: usize) -> Self {
        Self { fetcher, provider, top_n }
    }

    pub fn provider_name(&self) -> &str {
        self.provider.name()
    }

    /// Search the web. `limit` defaults to the configured `top_n`, clamped to a
    /// sane range so a caller can't ask a provider for thousands of results.
    pub async fn search(&self, query: &str, limit: Option<usize>) -> anyhow::Result<Vec<SearchResult>> {
        if query.trim().is_empty() {
            anyhow::bail!("empty query");
        }
        let n = limit.unwrap_or(self.top_n).clamp(1, 50);
        self.provider.search(self.fetcher.as_ref(), query, n).await
    }

    /// Fetch a URL (robots-gated, polite) and return it as a reader-mode [`Page`].
    pub async fn fetch(&self, url: &str) -> anyhow::Result<Page> {
        let resp = self.fetcher.get(url).await?;
        Ok(extract_bytes(&resp.body, &resp.final_url))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::{FetchResponse, Source};
    use async_trait::async_trait;

    /// A fetcher that returns a fixed body for every request (no network).
    struct Canned {
        body:         Vec<u8>,
        content_type: Option<String>,
    }
    #[async_trait]
    impl Fetcher for Canned {
        async fn get(&self, url: &str) -> anyhow::Result<FetchResponse> {
            Ok(FetchResponse {
                final_url:    url.to_string(),
                status:       200,
                content_type: self.content_type.clone(),
                body:         self.body.clone(),
                source:       Source::Network,
            })
        }
    }

    fn engine_with(body: &str, provider: Box<dyn SearchProvider>) -> Engine {
        let fetcher = Arc::new(Canned { body: body.as_bytes().to_vec(), content_type: None });
        Engine::with_parts(fetcher, provider, 5)
    }

    #[tokio::test]
    async fn search_returns_ranked_provider_results() {
        let ddg = r#"<div class="result"><div class="links_main">
            <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Frust-lang.org%2F">Rust</a>
            <a class="result__snippet">A safe systems language.</a></div></div>"#;
        let engine = engine_with(ddg, Box::new(crate::providers::DuckDuckGo));
        let results = engine.search("rust", None).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://rust-lang.org/");
        assert_eq!(results[0].rank, 0);
    }

    #[tokio::test]
    async fn empty_query_is_rejected() {
        let engine = engine_with("", Box::new(crate::providers::DuckDuckGo));
        assert!(engine.search("   ", None).await.is_err());
    }

    #[tokio::test]
    async fn fetch_returns_reader_mode_markdown() {
        let html = "<html><head><title>T</title></head><body><main>\
                    <h1>Heading</h1><p>Body with a <a href=\"/x\">link</a>.</p></main></body></html>";
        let engine = engine_with(html, Box::new(crate::providers::DuckDuckGo));
        let page = engine.fetch("https://example.com/post").await.unwrap();
        assert_eq!(page.title.as_deref(), Some("T"));
        assert!(page.markdown.contains("# Heading"));
        assert_eq!(page.links[0].url, "https://example.com/x", "relative link resolved");
    }
}
