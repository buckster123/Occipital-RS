//! The Engine — what every binary drives. Owns the polite fetcher, the selected
//! search provider, and the read-through cache, and exposes the verbs the tool
//! surface needs: `search`, `fetch`, `save`, `forget`. Freshness *policy* (the
//! TTL) lives here; the cache only records `fetched_at`.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::cache::Cache;
use crate::config::Config;
use crate::embed::{cosine, make_embedder, Embedder};
use crate::extract::{extract_bytes, Page};
use crate::fetch::{Fetcher, PoliteFetcher};
use crate::providers::{provider_for, SearchProvider, SearchResult};

/// One hit from `web_recall` over already-read pages.
#[derive(Debug, Clone, Serialize)]
pub struct RecallHit {
    pub url:     String,
    pub title:   Option<String>,
    pub snippet: String,
    /// Cosine score (semantic recall) or `None` (FTS5 keyword recall).
    pub score:   Option<f32>,
}

pub struct Engine {
    fetcher:        Arc<dyn Fetcher>,
    provider:       Box<dyn SearchProvider>,
    cache:          Option<Arc<Cache>>,
    embedder:       Option<Arc<dyn Embedder>>,
    top_n:          usize,
    fresh_ttl_secs: u64,
}

impl Engine {
    /// Build the production engine: polite fetcher + config provider + on-disk cache.
    pub fn from_config(cfg: &Config) -> anyhow::Result<Self> {
        let fetcher: Arc<dyn Fetcher> = Arc::new(PoliteFetcher::new(cfg)?);
        let cache = match Cache::open(&cfg.db_path) {
            Ok(c) => Some(Arc::new(c)),
            Err(e) => {
                tracing::warn!("cache disabled (open failed): {e}");
                None
            }
        };
        Ok(Self {
            fetcher,
            provider: provider_for(cfg),
            cache,
            embedder: make_embedder(&cfg.embed_model),
            top_n: cfg.search_top_n,
            fresh_ttl_secs: cfg.fresh_ttl_secs,
        })
    }

    /// Inject parts directly (tests / custom embeddings). Embedder defaults to
    /// none (FTS5 recall) — add one with [`with_embedder`](Self::with_embedder).
    pub fn with_parts(
        fetcher: Arc<dyn Fetcher>,
        provider: Box<dyn SearchProvider>,
        cache: Option<Arc<Cache>>,
        top_n: usize,
        fresh_ttl_secs: u64,
    ) -> Self {
        Self { fetcher, provider, cache, embedder: None, top_n, fresh_ttl_secs }
    }

    pub fn with_embedder(mut self, embedder: Option<Arc<dyn Embedder>>) -> Self {
        self.embedder = embedder;
        self
    }

    pub fn provider_name(&self) -> &str {
        self.provider.name()
    }

    /// Store a freshly-fetched page in the cache + (if embeddings are on) index
    /// its vector. No-op without a cache. Sync — never holds a lock across await.
    fn index(&self, page: &Page, etag: Option<&str>, last_modified: Option<&str>, pinned: bool) -> anyhow::Result<()> {
        let Some(cache) = &self.cache else { return Ok(()) };
        cache.put_page(page, etag, last_modified, pinned)?;
        if let Some(embedder) = &self.embedder {
            match embedder.embed(&embed_text(page)) {
                Ok(v) => {
                    if let Err(e) = cache.put_embedding(&page.url, &v) {
                        tracing::warn!("embedding store failed for {}: {e}", page.url);
                    }
                }
                Err(e) => tracing::warn!("embed failed for {}: {e}", page.url),
            }
        }
        Ok(())
    }

    fn is_fresh(&self, fetched_at: DateTime<Utc>) -> bool {
        Utc::now().signed_duration_since(fetched_at) < chrono::Duration::seconds(self.fresh_ttl_secs as i64)
    }

    /// Search the web, cache-first. Returns `(results, from_cache)`. A re-asked
    /// query inside the freshness window makes **zero** live requests. `fresh`
    /// forces a live search (still written back to the cache).
    pub async fn search(
        &self,
        query: &str,
        limit: Option<usize>,
        fresh: bool,
    ) -> anyhow::Result<(Vec<SearchResult>, bool)> {
        if query.trim().is_empty() {
            anyhow::bail!("empty query");
        }
        let n = limit.unwrap_or(self.top_n).clamp(1, 50);
        let key = search_key(self.provider.name(), query, n);

        if !fresh {
            if let Some(cache) = &self.cache {
                if let Some((results, ts)) = cache.get_search(&key)? {
                    if self.is_fresh(ts) {
                        return Ok((results, true));
                    }
                }
            }
        }
        let results = self.provider.search(self.fetcher.as_ref(), query, n).await?;
        if let Some(cache) = &self.cache {
            cache.put_search(&key, query, &results)?;
        }
        Ok((results, false))
    }

    /// Fetch a URL as a reader-mode page, cache-first. Returns `(page, from_cache)`.
    /// A fresh hit is served from the cache; a stale entry is refreshed with a
    /// conditional GET (a `304` keeps the cached body). `fresh` forces a live fetch.
    pub async fn fetch(&self, url: &str, fresh: bool) -> anyhow::Result<(Page, bool)> {
        let existing = match &self.cache {
            Some(c) => c.get_page(url)?,
            None => None,
        };

        if !fresh {
            if let Some(row) = &existing {
                if self.is_fresh(row.fetched_at) {
                    if let Some(c) = &self.cache {
                        c.touch_page(url)?;
                    }
                    return Ok((row.page.clone(), true));
                }
                // Stale → conditional refresh.
                let resp = self
                    .fetcher
                    .get_conditional(url, row.etag.as_deref(), row.last_modified.as_deref())
                    .await?;
                if resp.status == 304 {
                    if let Some(c) = &self.cache {
                        c.mark_fresh(url)?;
                    }
                    return Ok((row.page.clone(), true));
                }
                let page = extract_bytes(&resp.body, &resp.final_url);
                self.index(&page, resp.etag.as_deref(), resp.last_modified.as_deref(), row.pinned)?;
                return Ok((page, false));
            }
        }

        // Miss or fresh-forced: live fetch, preserving any existing pin.
        let resp = self.fetcher.get(url).await?;
        let page = extract_bytes(&resp.body, &resp.final_url);
        let pinned = existing.as_ref().map(|r| r.pinned).unwrap_or(false);
        self.index(&page, resp.etag.as_deref(), resp.last_modified.as_deref(), pinned)?;
        Ok((page, false))
    }

    /// Fetch + pin a URL (exempt from decay-based eviction until its TTL).
    pub async fn save(&self, url: &str) -> anyhow::Result<Page> {
        let resp = self.fetcher.get(url).await?;
        let page = extract_bytes(&resp.body, &resp.final_url);
        self.index(&page, resp.etag.as_deref(), resp.last_modified.as_deref(), true)?;
        Ok(page)
    }

    /// Recall over **already-read** pages only (no live web). Semantic (cosine)
    /// when embeddings are on, FTS5 keyword otherwise. Returns ranked hits.
    pub async fn recall(&self, query: &str, limit: Option<usize>) -> anyhow::Result<Vec<RecallHit>> {
        if query.trim().is_empty() {
            anyhow::bail!("empty query");
        }
        let n = limit.unwrap_or(self.top_n).clamp(1, 50);
        let Some(cache) = &self.cache else { return Ok(Vec::new()) };

        let ranked: Vec<(String, Option<f32>)> = if let Some(embedder) = &self.embedder {
            let qv = embedder.embed(query)?;
            let mut scored: Vec<(String, f32)> =
                cache.all_embeddings()?.into_iter().map(|(u, v)| (u, cosine(&qv, &v))).collect();
            scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            scored.truncate(n);
            scored.into_iter().map(|(u, s)| (u, Some(s))).collect()
        } else {
            cache.keyword_search(query, n)?.into_iter().map(|u| (u, None)).collect()
        };

        let mut hits = Vec::new();
        for (url, score) in ranked {
            if let Some(row) = cache.get_page(&url)? {
                hits.push(RecallHit {
                    url,
                    title: row.page.title,
                    snippet: snippet(&row.page.markdown),
                    score,
                });
            }
        }
        Ok(hits)
    }

    /// Evict a URL from the cache. `false` if it wasn't cached (or no cache).
    pub fn forget(&self, url: &str) -> anyhow::Result<bool> {
        match &self.cache {
            Some(c) => c.delete_page(url),
            None => Ok(false),
        }
    }
}

/// Cache key for a search: provider + limit + normalized query, so the same ask
/// hits regardless of casing/whitespace.
fn search_key(provider: &str, query: &str, limit: usize) -> String {
    format!("{provider}|{limit}|{}", query.trim().to_lowercase())
}

/// Text fed to the embedder: title + body, capped (bge truncates internally, but
/// a bounded input keeps embedding cheap). Char-boundary safe.
fn embed_text(page: &Page) -> String {
    let mut t = page.title.clone().unwrap_or_default();
    t.push('\n');
    t.push_str(&page.markdown);
    truncate_chars(&t, 2000).to_string()
}

/// A short preview of a page body for a recall hit.
fn snippet(markdown: &str) -> String {
    truncate_chars(markdown.trim(), 220).to_string()
}

fn truncate_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::{FetchResponse, Source};
    use crate::providers::DuckDuckGo;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts every network call, and can simulate a 304 on a conditional GET.
    struct Counting {
        body:           Vec<u8>,
        calls:          AtomicUsize,
        conditional_304: bool,
    }
    impl Counting {
        fn new(body: &str) -> Arc<Self> {
            Arc::new(Self { body: body.as_bytes().to_vec(), calls: AtomicUsize::new(0), conditional_304: false })
        }
        fn with_304(body: &str) -> Arc<Self> {
            Arc::new(Self { body: body.as_bytes().to_vec(), calls: AtomicUsize::new(0), conditional_304: true })
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
        fn resp(&self, url: &str, status: u16, body: Vec<u8>) -> FetchResponse {
            FetchResponse { final_url: url.into(), status, content_type: None, etag: Some("v1".into()), last_modified: None, body, source: Source::Network }
        }
    }
    #[async_trait]
    impl Fetcher for Counting {
        async fn get(&self, url: &str) -> anyhow::Result<FetchResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.resp(url, 200, self.body.clone()))
        }
        async fn get_conditional(&self, url: &str, _e: Option<&str>, _l: Option<&str>) -> anyhow::Result<FetchResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.conditional_304 {
                Ok(self.resp(url, 304, Vec::new()))
            } else {
                Ok(self.resp(url, 200, self.body.clone()))
            }
        }
    }

    fn engine(fetcher: Arc<dyn Fetcher>, ttl: u64) -> Engine {
        let cache = Arc::new(Cache::open_in_memory().unwrap());
        Engine::with_parts(fetcher, Box::new(DuckDuckGo), Some(cache), 5, ttl)
    }

    const HTML: &str = "<html><head><title>T</title></head><body><main><h1>H</h1><p>b</p></main></body></html>";

    #[tokio::test]
    async fn second_fetch_is_served_from_cache() {
        let f = Counting::new(HTML);
        let e = engine(f.clone(), 3600);
        let (_p1, c1) = e.fetch("https://e.test/", false).await.unwrap();
        let (_p2, c2) = e.fetch("https://e.test/", false).await.unwrap();
        assert!(!c1 && c2, "first live, second from cache");
        assert_eq!(f.calls(), 1, "the cache hit made zero live requests");
    }

    #[tokio::test]
    async fn stale_entry_refreshes_via_304() {
        let f = Counting::with_304(HTML);
        let e = engine(f.clone(), 0); // ttl 0 → always stale
        let (_p1, c1) = e.fetch("https://e.test/", false).await.unwrap();
        let (_p2, c2) = e.fetch("https://e.test/", false).await.unwrap();
        assert!(!c1, "first is a miss");
        assert!(c2, "a 304 keeps the cached body (served from cache)");
        assert_eq!(f.calls(), 2, "one initial GET + one cheap conditional GET");
    }

    #[tokio::test]
    async fn fresh_flag_forces_a_live_fetch() {
        let f = Counting::new(HTML);
        let e = engine(f.clone(), 3600);
        e.fetch("https://e.test/", false).await.unwrap();
        let (_p, from_cache) = e.fetch("https://e.test/", true).await.unwrap();
        assert!(!from_cache, "fresh=true bypasses the cache");
        assert_eq!(f.calls(), 2);
    }

    #[tokio::test]
    async fn reasked_search_makes_zero_live_requests() {
        let ddg = r#"<div class="result"><div class="links_main">
            <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Frust-lang.org%2F">Rust</a></div></div>"#;
        let f = Counting::new(ddg);
        let e = engine(f.clone(), 3600);
        let (r1, c1) = e.search("Rust", None, false).await.unwrap();
        let (r2, c2) = e.search("  rust  ", None, false).await.unwrap(); // normalized → same key
        assert!(!c1 && c2, "first live, second cached");
        assert_eq!(r1, r2);
        assert_eq!(f.calls(), 1, "the re-asked search hit zero live requests");
    }

    #[tokio::test]
    async fn save_pins_and_forget_evicts() {
        let f = Counting::new(HTML);
        let e = engine(f.clone(), 3600);
        e.save("https://e.test/").await.unwrap();
        assert!(e.forget("https://e.test/").unwrap(), "saved page can be forgotten");
        assert!(!e.forget("https://e.test/").unwrap(), "second forget is a no-op");
    }

    // ---- recall ----------------------------------------------------------

    use crate::embed::{BagOfWordsEmbedder, Embedder};
    use crate::extract::Page;

    fn mkpage(url: &str, body: &str) -> Page {
        Page {
            url: url.into(),
            title: Some(url.into()),
            byline: None,
            markdown: body.into(),
            links: vec![],
            content_hash: "h".into(),
        }
    }

    /// Build a cache-only engine (fetcher unused) and populate the cache + (if
    /// `embedder`) the vectors directly — recall reads only the cache.
    fn recall_engine(pages: &[(&str, &str)], embedder: Option<Arc<dyn Embedder>>) -> Engine {
        let cache = Arc::new(Cache::open_in_memory().unwrap());
        for (url, body) in pages {
            let page = mkpage(url, body);
            cache.put_page(&page, None, None, false).unwrap();
            if let Some(e) = &embedder {
                cache.put_embedding(url, &e.embed(body).unwrap()).unwrap();
            }
        }
        Engine::with_parts(Counting::new(""), Box::new(DuckDuckGo), Some(cache), 5, 3600)
            .with_embedder(embedder)
    }

    #[tokio::test]
    async fn recall_semantic_ranks_by_similarity() {
        let emb: Arc<dyn Embedder> = Arc::new(BagOfWordsEmbedder::new());
        let e = recall_engine(
            &[
                ("https://e.test/rust", "rust async await tokio runtime concurrency"),
                ("https://e.test/bread", "sourdough bread baking flour yeast oven"),
            ],
            Some(emb),
        );
        let hits = e.recall("async rust tokio", Some(2)).await.unwrap();
        assert_eq!(hits[0].url, "https://e.test/rust", "most similar page ranks first");
        assert!(hits[0].score.unwrap() > hits[1].score.unwrap(), "scored, descending");
    }

    #[tokio::test]
    async fn recall_keyword_fallback_without_embedder() {
        let e = recall_engine(
            &[
                ("https://e.test/rust", "the rust programming language"),
                ("https://e.test/bread", "sourdough bread baking"),
            ],
            None, // Nano: FTS5 keyword recall
        );
        let hits = e.recall("rust language", Some(5)).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://e.test/rust");
        assert!(hits[0].score.is_none(), "keyword recall has no cosine score");
    }
}
