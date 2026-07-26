//! The Engine — what every binary drives. Owns the polite fetcher, the selected
//! search provider, and the read-through cache, and exposes the verbs the tool
//! surface needs: `search`, `fetch`, `save`, `forget`. Freshness *policy* (the
//! TTL) lives here; the cache only records `fetched_at`.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::cache::{Cache, CacheLog, RequestRow};
use crate::config::{
    Config, DEFAULT_DECAY_HALFLIFE_SECS, DEFAULT_GC_MIN_AGE_SECS, DEFAULT_GC_MIN_SALIENCE,
    DEFAULT_SNAPSHOT_TTL_SECS,
};
use crate::curate::{make_auto_distiller, make_distiller, Distiller};
use crate::decay::{decay_factor, effective_salience};
use crate::embed::{cosine, make_embedder, Embedder};
use crate::extract::{extract_bytes, Form, Page};
use crate::fetch::{Fetcher, HttpRequest, PoliteFetcher};
use crate::keys::Keys;
use crate::providers::{provider_for, SearchProvider, SearchResult};

/// One hit from `web_recall` over already-read pages.
#[derive(Debug, Clone, Serialize)]
pub struct RecallHit {
    pub url:     String,
    pub title:   Option<String>,
    /// The distilled summary when the page is curated, else a raw-body preview.
    pub snippet: String,
    /// Cosine score (semantic recall) or `None` (FTS5 keyword recall).
    pub score:   Option<f32>,
    /// Distilled topic tags (empty when the page isn't curated yet).
    pub tags:    Vec<String>,
    /// Whether `snippet` is curated knowledge rather than a raw preview.
    pub distilled: bool,
}

/// One page successfully distilled by [`Engine::distill`].
#[derive(Debug, Clone, Serialize)]
pub struct DistilledPage {
    pub url:        String,
    pub title:      Option<String>,
    pub summary:    String,
    pub key_points: Vec<String>,
    pub entities:   Vec<String>,
    pub tags:       Vec<String>,
    pub model:      String,
    pub backend:    String,
    /// `true` when a current distillation already existed (no LLM call made).
    pub from_cache: bool,
}

/// One page that failed to distill (the sweep continues past failures).
#[derive(Debug, Clone, Serialize)]
pub struct DistillFailure {
    pub url:   String,
    pub error: String,
}

/// The outcome of a distill run (single URL or sweep).
#[derive(Debug, Clone, Serialize)]
pub struct DistillReport {
    pub distilled: Vec<DistilledPage>,
    pub failed:    Vec<DistillFailure>,
    /// Cached pages still awaiting distillation after this run.
    pub remaining: usize,
}

/// Default pages per no-URL distill sweep — bounded so one call stays cheap
/// (each page is an LLM call); `limit` overrides within [1, 10].
const DEFAULT_DISTILL_SWEEP: usize = 3;

/// A link with its stable 1-based ordinal — the handle the interaction verbs
/// (Phase 13) will click by.
#[derive(Debug, Clone, Serialize)]
pub struct IndexedLink {
    pub idx:  usize,
    pub text: String,
    pub url:  String,
}

/// The element registry for one page (`web_dom`): links + forms with stable
/// ordinals, and whether a raw-HTML snapshot is currently held (i.e. whether
/// an interaction could resolve against it without a re-fetch).
#[derive(Debug, Clone, Serialize)]
pub struct DomView {
    pub url:          String,
    pub title:        Option<String>,
    pub links:        Vec<IndexedLink>,
    pub forms:        Vec<Form>,
    pub content_hash: String,
    pub from_cache:   bool,
    pub snapshot:     bool,
    /// The page's content was mined from embedded data (the registry may be
    /// leaner than the rendered app would show).
    pub salvaged:     bool,
    /// Client-only page — the registry is what static HTML yields, no more.
    pub js_required:  bool,
}

/// What an element selector (`web_click`) addresses: `link:N` or `form:N`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ElementSel {
    Link(usize),
    Form(usize),
}

/// Parse the click micro-grammar: `link:3` / `form:1` (1-based ordinals from
/// the element registry).
fn parse_element(s: &str) -> anyhow::Result<ElementSel> {
    let t = s.trim().to_ascii_lowercase();
    let (kind, n) = t
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("element must be link:N or form:N (got {s:?})"))?;
    let idx: usize = n
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("element ordinal must be a number (got {s:?})"))?;
    if idx == 0 {
        anyhow::bail!("element ordinals are 1-based (got {s:?})");
    }
    match kind.trim() {
        "link" => Ok(ElementSel::Link(idx)),
        "form" => Ok(ElementSel::Form(idx)),
        other => anyhow::bail!("unknown element kind {other:?} — use link:N or form:N"),
    }
}

/// One field as actually submitted (`password` values redacted for the report;
/// the wire carries the real value).
#[derive(Debug, Clone, Serialize)]
pub struct SentField {
    pub name:  String,
    pub value: String,
}

/// The outcome of `web_click`: the element resolved, the navigation made, and
/// the resulting reader-mode page.
#[derive(Debug, Clone, Serialize)]
pub struct ClickReport {
    pub source_url: String,
    pub element:    String,
    /// The link href followed, or the clicked form's action.
    pub target_url: String,
    pub page:       Page,
    pub from_cache: bool,
    /// HTTP status of a POST-form click (`None` for the read-through paths,
    /// where status is absorbed by the fetch pipeline).
    pub status:     Option<u16>,
    /// Set when the result page is NOT addressable by URL (a POST result):
    /// pass it as the `url` of the next `web_dom`/`web_click`/`web_submit`
    /// to resolve ordinals against THIS page. See [`ResultStore`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handle:     Option<String>,
}

/// The outcome of `web_submit`: what was sent, where, and the resulting page.
#[derive(Debug, Clone, Serialize)]
pub struct SubmitReport {
    pub source_url: String,
    pub form:       usize,
    pub action:     String,
    pub method:     String,
    pub sent:       Vec<SentField>,
    /// HTTP status of a live POST (`None` on the GET path — served through the
    /// normal read-through pipeline).
    pub status:     Option<u16>,
    pub page:       Page,
    /// GET results cache like any page; a POST result is **never** cached (the
    /// URL alone cannot reproduce it).
    pub cached:     bool,
    /// Set when the result page is NOT addressable by URL (a POST result) —
    /// the ordinal-resolution handle for the next verb. See [`ResultStore`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handle:     Option<String>,
}

/// A POST result is deliberately never in the durable page cache (the URL
/// alone cannot reproduce it) — but without SOME address, its ordinals are
/// unresolvable and interaction depth caps at a single POST hop (apex1 field
/// report, 2026-07-26: a POST-paginated SERP was walkable to page 2 and no
/// further). This store is the middle ground: **working memory, not
/// knowledge** — the last few interaction results, held in memory only under
/// an opaque `result:<hash>` handle, bounded and TTL'd, gone on restart. The
/// durable cache's "POST is never cached" invariant stands untouched.
const RESULT_STORE_CAP: usize = 16;
const RESULT_TTL_SECS: u64 = 900; // a browsing session, not knowledge

#[derive(Default)]
struct ResultStore {
    /// Insertion-ordered (oldest first); tiny, linear scans are fine.
    entries: Vec<(String, StoredResult)>,
}

struct StoredResult {
    page: Page,
    at:   std::time::Instant,
}

impl ResultStore {
    fn put(&mut self, handle: String, page: Page) {
        self.entries.retain(|(h, r)| h != &handle && r.at.elapsed().as_secs() < RESULT_TTL_SECS);
        self.entries.push((handle, StoredResult { page, at: std::time::Instant::now() }));
        while self.entries.len() > RESULT_STORE_CAP {
            self.entries.remove(0);
        }
    }

    fn get(&self, handle: &str) -> Option<Page> {
        self.entries
            .iter()
            .find(|(h, r)| h == handle && r.at.elapsed().as_secs() < RESULT_TTL_SECS)
            .map(|(_, r)| r.page.clone())
    }
}

pub struct Engine {
    fetcher:            Arc<dyn Fetcher>,
    provider:           Box<dyn SearchProvider>,
    cache:              Option<Arc<Cache>>,
    embedder:           Option<Arc<dyn Embedder>>,
    distiller:          Option<Arc<dyn Distiller>>,
    /// The background-sweep distiller (`OCCIPITAL_AUTO_DISTILL`) — may differ
    /// from `distiller` (`local` pins it to Ollama-only). `None` = auto off.
    auto_distiller:     Option<Arc<dyn Distiller>>,
    /// Rolling-24h distillation budget for the background sweep (0 = uncapped).
    auto_cap:           usize,
    top_n:              usize,
    fresh_ttl_secs:     u64,
    decay_half_life_secs: u64,
    gc_min_salience:    f32,
    gc_min_age_secs:    u64,
    snapshot_ttl_secs:  u64,
    /// In-memory POST-result pages, keyed by `result:<hash>` handle.
    results:            std::sync::Mutex<ResultStore>,
}

impl Engine {
    /// Build the production engine: polite fetcher + config provider + on-disk cache.
    pub fn from_config(cfg: &Config) -> anyhow::Result<Self> {
        let cache = match Cache::open(&cfg.db_path) {
            Ok(c) => Some(Arc::new(c)),
            Err(e) => {
                tracing::warn!("cache disabled (open failed): {e}");
                None
            }
        };
        // The request log lives in the cache; `fetch` only knows the sink seam.
        let sink: Option<Arc<dyn crate::fetch::RequestSink>> = match (&cache, cfg.log_max) {
            (Some(c), max) if max > 0 => Some(Arc::new(CacheLog::new(c.clone(), max))),
            _ => None,
        };
        let fetcher: Arc<dyn Fetcher> = Arc::new(PoliteFetcher::new(cfg)?.with_sink(sink));
        let keys = Keys::load(&cfg.keys_file);
        Ok(Self {
            fetcher,
            provider: provider_for(cfg, &keys),
            cache,
            embedder: make_embedder(&cfg.embed_model),
            distiller: make_distiller(&cfg.curate),
            auto_distiller: make_auto_distiller(&cfg.curate),
            auto_cap: cfg.curate.auto_cap,
            top_n: cfg.search_top_n,
            fresh_ttl_secs: cfg.fresh_ttl_secs,
            decay_half_life_secs: cfg.decay_half_life_secs,
            gc_min_salience: cfg.gc_min_salience,
            gc_min_age_secs: cfg.gc_min_age_secs,
            snapshot_ttl_secs: cfg.snapshot_ttl_secs,
            results: std::sync::Mutex::new(ResultStore::default()),
        })
    }

    /// Inject parts directly (tests / custom embeddings). Embedder defaults to
    /// none (FTS5 recall) — add one with [`with_embedder`](Self::with_embedder);
    /// decay/GC default to the config defaults — override with
    /// [`with_gc_params`](Self::with_gc_params).
    pub fn with_parts(
        fetcher: Arc<dyn Fetcher>,
        provider: Box<dyn SearchProvider>,
        cache: Option<Arc<Cache>>,
        top_n: usize,
        fresh_ttl_secs: u64,
    ) -> Self {
        Self {
            fetcher,
            provider,
            cache,
            embedder: None,
            distiller: None,
            auto_distiller: None,
            auto_cap: 0,
            top_n,
            fresh_ttl_secs,
            decay_half_life_secs: DEFAULT_DECAY_HALFLIFE_SECS,
            gc_min_salience: DEFAULT_GC_MIN_SALIENCE,
            gc_min_age_secs: DEFAULT_GC_MIN_AGE_SECS,
            snapshot_ttl_secs: DEFAULT_SNAPSHOT_TTL_SECS,
            results: std::sync::Mutex::new(ResultStore::default()),
        }
    }

    pub fn with_embedder(mut self, embedder: Option<Arc<dyn Embedder>>) -> Self {
        self.embedder = embedder;
        self
    }

    pub fn with_distiller(mut self, distiller: Option<Arc<dyn Distiller>>) -> Self {
        self.distiller = distiller;
        self
    }

    pub fn with_auto_distiller(mut self, distiller: Option<Arc<dyn Distiller>>, cap: usize) -> Self {
        self.auto_distiller = distiller;
        self.auto_cap = cap;
        self
    }

    pub fn with_gc_params(mut self, half_life_secs: u64, min_salience: f32, min_age_secs: u64) -> Self {
        self.decay_half_life_secs = half_life_secs;
        self.gc_min_salience = min_salience;
        self.gc_min_age_secs = min_age_secs;
        self
    }

    pub fn provider_name(&self) -> &str {
        self.provider.name()
    }

    /// Store a freshly-fetched page in the cache — its extracted form, a
    /// raw-HTML snapshot (the interaction working memory), and (if embeddings
    /// are on) its vector. No-op without a cache. Sync — never holds a lock
    /// across await.
    fn index(&self, page: &Page, html: &str, etag: Option<&str>, last_modified: Option<&str>, pinned: bool) -> anyhow::Result<()> {
        let Some(cache) = &self.cache else { return Ok(()) };
        cache.put_page(page, etag, last_modified, pinned)?;
        if let Err(e) = cache.put_snapshot(&page.url, html) {
            tracing::warn!("snapshot store failed for {}: {e}", page.url);
        }
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
                let html = String::from_utf8_lossy(&resp.body);
                self.index(&page, &html, resp.etag.as_deref(), resp.last_modified.as_deref(), row.pinned)?;
                return Ok((page, false));
            }
        }

        // Miss or fresh-forced: live fetch, preserving any existing pin.
        let resp = self.fetcher.get(url).await?;
        let page = extract_bytes(&resp.body, &resp.final_url);
        let html = String::from_utf8_lossy(&resp.body);
        let pinned = existing.as_ref().map(|r| r.pinned).unwrap_or(false);
        self.index(&page, &html, resp.etag.as_deref(), resp.last_modified.as_deref(), pinned)?;
        Ok((page, false))
    }

    /// Fetch + pin a URL (exempt from decay-based eviction until its TTL).
    pub async fn save(&self, url: &str) -> anyhow::Result<Page> {
        let resp = self.fetcher.get(url).await?;
        let page = extract_bytes(&resp.body, &resp.final_url);
        let html = String::from_utf8_lossy(&resp.body);
        self.index(&page, &html, resp.etag.as_deref(), resp.last_modified.as_deref(), true)?;
        Ok(page)
    }

    /// The element registry for a page (`web_dom`): links + forms with stable
    /// ordinals. Cache-first exactly like `fetch` (an uncached URL is fetched
    /// and stored); `snapshot` reports whether a raw-HTML snapshot is held and
    /// inside its TTL — i.e. whether an interaction verb could resolve these
    /// ordinals without a re-fetch.
    /// Resolve the SOURCE page an interaction addresses: a `result:<hash>`
    /// handle looks up the in-memory result store (no network, ever); anything
    /// else goes through the normal cache-first `fetch`. The `bool` mirrors
    /// `fetch`'s from_cache (a handle hit reports `true` — zero live requests).
    async fn resolve_source(&self, url: &str) -> anyhow::Result<(Page, bool)> {
        if url.starts_with("result:") {
            let page = self.results.lock().expect("result store lock").get(url);
            return page.map(|p| (p, true)).ok_or_else(|| {
                anyhow::anyhow!(
                    "unknown or expired result handle {url} — interaction results live \
                     ~{} min in working memory; re-run the web_submit/web_click that \
                     produced it, or browse via a GET URL (addressable + cached)",
                    RESULT_TTL_SECS / 60
                )
            });
        }
        self.fetch(url, false).await
    }

    /// Store a POST-obtained page in the result store and mint its handle.
    fn store_result(&self, source_url: &str, form_idx: usize, page: &Page) -> String {
        let key = format!("{source_url}|{form_idx}|{}", page.content_hash);
        let handle = format!("result:{}", crate::extract::fnv1a_hex(key.as_bytes()));
        self.results.lock().expect("result store lock").put(handle.clone(), page.clone());
        handle
    }

    pub async fn dom(&self, url: &str, fresh: bool) -> anyhow::Result<DomView> {
        let (page, from_cache) = if url.starts_with("result:") {
            self.resolve_source(url).await?
        } else {
            self.fetch(url, fresh).await?
        };
        let snapshot = self.snapshot_fresh(&page.url)?;
        Ok(DomView {
            links: page
                .links
                .iter()
                .enumerate()
                .map(|(i, l)| IndexedLink { idx: i + 1, text: l.text.clone(), url: l.url.clone() })
                .collect(),
            forms: page.forms,
            url: page.url,
            title: page.title,
            content_hash: page.content_hash,
            from_cache,
            snapshot,
            salvaged: page.salvaged,
            js_required: page.js_required,
        })
    }

    /// Click an element on a page by its registry ordinal (`link:N` / `form:N`).
    /// A link click is a polite GET of its href through the normal read-through
    /// pipeline; a form click submits that form with its current values (see
    /// [`submit`](Self::submit)). The source page resolves cache-first
    /// (fetch-if-uncached), same as `web_dom`.
    pub async fn click(&self, url: &str, element: &str) -> anyhow::Result<ClickReport> {
        match parse_element(element)? {
            ElementSel::Link(n) => {
                let (page, _) = self.resolve_source(url).await?;
                let link = page.links.get(n - 1).ok_or_else(|| {
                    anyhow::anyhow!("no link #{n} on {} ({} links in the registry — see web_dom)", page.url, page.links.len())
                })?;
                let (target, from_cache) = self.fetch(&link.url, false).await?;
                Ok(ClickReport {
                    source_url: page.url.clone(),
                    element: element.trim().to_string(),
                    target_url: link.url.clone(),
                    page: target,
                    from_cache,
                    status: None,
                    handle: None, // a followed link is a GET — addressable by its URL
                })
            }
            ElementSel::Form(n) => {
                let report = self.submit(url, n, &[]).await?;
                Ok(ClickReport {
                    source_url: report.source_url,
                    element: element.trim().to_string(),
                    target_url: report.action,
                    page: report.page,
                    from_cache: report.cached,
                    status: report.status,
                    handle: report.handle,
                })
            }
        }
    }

    /// Fill and submit a form by its registry ordinal. `overrides` set named
    /// fields; everything else keeps its current value (hidden state verbatim,
    /// exactly as the site sent it). GET submits through the read-through
    /// pipeline (a repeated identical submission is a cache hit — zero live
    /// requests); POST goes live once, is **never auto-retried**, and its
    /// result is never cached. Politeness gates (robots, rate, concurrency)
    /// apply to both.
    pub async fn submit(
        &self,
        url: &str,
        form_idx: usize,
        overrides: &[(String, String)],
    ) -> anyhow::Result<SubmitReport> {
        let (page, _) = self.resolve_source(url).await?;
        let form = page.forms.iter().find(|f| f.idx == form_idx).ok_or_else(|| {
            anyhow::anyhow!("no form #{form_idx} on {} ({} forms in the registry — see web_dom)", page.url, page.forms.len())
        })?;

        // An override naming a field the form doesn't have is almost certainly
        // a typo — refuse rather than silently misfire.
        for (name, _) in overrides {
            if !form.fields.iter().any(|f| &f.name == name) {
                let known: Vec<&str> = form.fields.iter()
                    .filter(|f| !f.name.is_empty()).map(|f| f.name.as_str()).collect();
                anyhow::bail!("form #{form_idx} has no field named {name:?} (fields: {known:?})");
            }
        }

        // Effective pairs, in the form's own field order: override > current
        // value > empty. An overridden name is sent once (radio groups repeat
        // names in the registry).
        let override_of = |name: &str| {
            overrides.iter().rev().find(|(n, _)| n == name).map(|(_, v)| v.clone())
        };
        let mut sent_names: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut pairs: Vec<(String, String)> = Vec::new();
        let mut redact: std::collections::HashSet<String> = std::collections::HashSet::new();
        for f in form.fields.iter().filter(|f| !f.name.is_empty()) {
            if f.kind == "password" {
                redact.insert(f.name.clone());
            }
            match override_of(&f.name) {
                Some(v) => {
                    if sent_names.insert(f.name.clone()) {
                        pairs.push((f.name.clone(), v));
                    }
                }
                None => pairs.push((f.name.clone(), f.value.clone().unwrap_or_default())),
            }
        }

        let sent: Vec<SentField> = pairs
            .iter()
            .map(|(n, v)| SentField {
                name:  n.clone(),
                value: if redact.contains(n) { "•••".to_string() } else { v.clone() },
            })
            .collect();

        let (action, method) = (form.action.clone(), form.method.clone());
        if method == "post" {
            let body = url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
                .finish();
            let resp = self.fetcher.request(HttpRequest::post_form(&action, body.into_bytes())).await?;
            let result = extract_bytes(&resp.body, &resp.final_url);
            // The result stays out of the durable cache (a POST is not
            // reproducible from its URL) but goes into the in-memory result
            // store, so the NEXT verb can address its ordinals via the handle
            // — without it, interaction depth caps at one POST hop.
            let handle = self.store_result(&page.url, form_idx, &result);
            Ok(SubmitReport {
                source_url: page.url,
                form: form_idx,
                action,
                method,
                sent,
                status: Some(resp.status),
                page: result,
                cached: false,
                handle: Some(handle),
            })
        } else {
            // Per the HTML spec a GET submission replaces the action's query
            // string with the form data.
            let mut u = url::Url::parse(&action)?;
            if pairs.is_empty() {
                u.set_query(None);
            } else {
                u.query_pairs_mut()
                    .clear()
                    .extend_pairs(pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())));
            }
            let (result, from_cache) = self.fetch(u.as_str(), false).await?;
            Ok(SubmitReport {
                source_url: page.url,
                form: form_idx,
                action,
                method,
                sent,
                status: None,
                page: result,
                cached: from_cache,
                handle: None, // GET results are addressable + cached by URL
            })
        }
    }

    /// Whether a snapshot exists for `url` and is inside the snapshot TTL.
    fn snapshot_fresh(&self, url: &str) -> anyhow::Result<bool> {
        let Some(cache) = &self.cache else { return Ok(false) };
        Ok(match cache.get_snapshot(url)? {
            Some((_, ts)) => {
                Utc::now().signed_duration_since(ts)
                    < chrono::Duration::seconds(self.snapshot_ttl_secs as i64)
            }
            None => false,
        })
    }

    /// Recall over **already-read** pages only (no live web). Semantic (cosine)
    /// when embeddings are on, FTS5 keyword otherwise. Relevance is scaled by a
    /// **disuse decay** factor, so a stale, long-unread page sinks beneath a
    /// fresher one of equal relevance. Returns ranked hits.
    pub async fn recall(&self, query: &str, limit: Option<usize>) -> anyhow::Result<Vec<RecallHit>> {
        if query.trim().is_empty() {
            anyhow::bail!("empty query");
        }
        let n = limit.unwrap_or(self.top_n).clamp(1, 50);
        let Some(cache) = &self.cache else { return Ok(Vec::new()) };

        let now = Utc::now();
        let half = self.decay_half_life_secs as f64;
        let disuse: HashMap<String, f64> = cache
            .all_page_meta()?
            .into_iter()
            .map(|m| (m.url, (now - m.last_access).num_seconds().max(0) as f64))
            .collect();
        let decay_of = |url: &str| decay_factor(*disuse.get(url).unwrap_or(&0.0), half);

        // (url, display_score, decayed_rank)
        let mut ranked: Vec<(String, Option<f32>, f64)> = if let Some(embedder) = &self.embedder {
            let qv = embedder.embed(query)?;
            cache
                .all_embeddings()?
                .into_iter()
                .map(|(u, v)| {
                    let cos = cosine(&qv, &v);
                    let rank = cos as f64 * decay_of(&u);
                    (u, Some(cos), rank)
                })
                .collect()
        } else {
            // Keyword matches all have relevance 1.0; decay orders them by recency.
            cache
                .keyword_search(query, 50)?
                .into_iter()
                .map(|u| {
                    let rank = decay_of(&u);
                    (u, None, rank)
                })
                .collect()
        };
        ranked.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        ranked.truncate(n);

        let mut hits = Vec::new();
        for (url, score, _) in ranked {
            if let Some(row) = cache.get_page(&url)? {
                // A curated page recalls as knowledge: the distilled summary +
                // tags, not a raw-body preview.
                let (snip, tags, distilled) = match cache.get_distillation(&url)? {
                    Some(d) => (d.summary, d.tags, true),
                    None => (snippet(&row.page.markdown), Vec::new(), false),
                };
                hits.push(RecallHit {
                    url,
                    title: row.page.title,
                    snippet: snip,
                    score,
                    tags,
                    distilled,
                });
            }
        }
        Ok(hits)
    }

    /// Distill cached pages into curated knowledge (summary, key points,
    /// entities, tags) via the configured LLM backend.
    ///
    /// With `url`: distill that page, fetching it first if uncached; a current
    /// distillation (matching page hash) is returned without an LLM call.
    /// Without: sweep up to `limit` (default 3, clamped 1–10) pages that were
    /// never distilled or whose content changed since. One page failing does
    /// not stop the sweep.
    pub async fn distill(
        &self,
        url: Option<&str>,
        limit: Option<usize>,
    ) -> anyhow::Result<DistillReport> {
        let Some(distiller) = &self.distiller else {
            anyhow::bail!("curation disabled (OCCIPITAL_CURATE_BACKEND=off)");
        };
        let Some(cache) = &self.cache else {
            anyhow::bail!("no cache — nothing to distill");
        };

        let mut distilled = Vec::new();
        let mut failed = Vec::new();

        let targets: Vec<String> = match url {
            Some(u) => {
                let u = u.trim();
                if u.is_empty() {
                    anyhow::bail!("empty url");
                }
                // Read-through: an uncached URL is fetched (and stored) first.
                let (page, _) = self.fetch(u, false).await?;
                // A current distillation is served as-is — an explicit re-ask
                // on unchanged content shouldn't re-spend an LLM call.
                if let Some(d) = cache.get_distillation(&page.url)? {
                    if d.content_hash == page.content_hash {
                        distilled.push(DistilledPage {
                            url:        page.url,
                            title:      page.title,
                            summary:    d.summary,
                            key_points: d.key_points,
                            entities:   d.entities,
                            tags:       d.tags,
                            model:      d.model.unwrap_or_default(),
                            backend:    "cache".into(),
                            from_cache: true,
                        });
                        let remaining = cache.undistilled_count()?;
                        return Ok(DistillReport { distilled, failed, remaining });
                    }
                }
                vec![page.url]
            }
            None => cache.undistilled_urls(limit.unwrap_or(DEFAULT_DISTILL_SWEEP).clamp(1, 10))?,
        };

        let (mut done, mut errs) = distill_targets(cache, distiller.as_ref(), targets).await?;
        distilled.append(&mut done);
        failed.append(&mut errs);

        let remaining = cache.undistilled_count()?;
        Ok(DistillReport { distilled, failed, remaining })
    }

    /// One background auto-curation sweep (the resident servers call this on an
    /// interval). Returns `None` when there is nothing to do: auto-distill off,
    /// no cache, the rolling-24h budget spent, or no pending pages. The batch is
    /// bounded by the sweep default AND the remaining budget.
    pub async fn auto_distill_tick(&self) -> anyhow::Result<Option<DistillReport>> {
        let Some(distiller) = &self.auto_distiller else { return Ok(None) };
        let Some(cache) = &self.cache else { return Ok(None) };

        let batch = if self.auto_cap == 0 {
            DEFAULT_DISTILL_SWEEP
        } else {
            let spent = cache.distilled_since(Utc::now() - chrono::Duration::hours(24))?;
            if spent >= self.auto_cap {
                tracing::debug!(spent, cap = self.auto_cap, "auto-distill budget spent — pausing");
                return Ok(None);
            }
            DEFAULT_DISTILL_SWEEP.min(self.auto_cap - spent)
        };

        let targets = cache.undistilled_urls(batch)?;
        if targets.is_empty() {
            return Ok(None);
        }
        let (distilled, failed) = distill_targets(cache, distiller.as_ref(), targets).await?;
        let remaining = cache.undistilled_count()?;
        Ok(Some(DistillReport { distilled, failed, remaining }))
    }

    /// Whether LLM curation is active (a distiller is configured).
    pub fn curation(&self) -> bool {
        self.distiller.is_some()
    }

    /// Whether background auto-curation is active (the servers gate their
    /// sweep task on this).
    pub fn auto_curation(&self) -> bool {
        self.auto_distiller.is_some()
    }

    /// Garbage-collect stale memory: prune unpinned pages whose **effective
    /// salience** (stored × disuse decay) has fallen below the floor, once they
    /// are older than the min-age. Pinned pages and recent fetches always
    /// survive. Returns the number pruned.
    pub fn gc(&self) -> anyhow::Result<usize> {
        let Some(cache) = &self.cache else { return Ok(0) };
        // Snapshots are working memory on a much shorter clock than pages —
        // expire them first, silently (they are not knowledge being forgotten).
        match cache.gc_snapshots(self.snapshot_ttl_secs) {
            Ok(n) if n > 0 => tracing::debug!(pruned = n, "expired interaction snapshots"),
            Ok(_) => {}
            Err(e) => tracing::warn!("snapshot gc failed: {e}"),
        }
        let now = Utc::now();
        let half = self.decay_half_life_secs as f64;
        let mut pruned = 0;
        for m in cache.all_page_meta()? {
            if m.pinned {
                continue;
            }
            let age = (now - m.fetched_at).num_seconds().max(0) as f64;
            if age < self.gc_min_age_secs as f64 {
                continue;
            }
            let disuse = (now - m.last_access).num_seconds().max(0) as f64;
            if effective_salience(m.salience, disuse, half) < self.gc_min_salience
                && cache.delete_page(&m.url)?
            {
                pruned += 1;
            }
        }
        Ok(pruned)
    }

    /// Evict a URL from the cache. `false` if it wasn't cached (or no cache).
    pub fn forget(&self, url: &str) -> anyhow::Result<bool> {
        match &self.cache {
            Some(c) => c.delete_page(url),
            None => Ok(false),
        }
    }

    /// Cache size counters (`None` if no cache).
    pub fn stats(&self) -> Option<crate::cache::CacheStats> {
        self.cache.as_ref().map(|c| c.stats())
    }

    /// The recent request trail (newest first) — what this node actually sent,
    /// what it cost, and what was refused.
    pub fn log(&self, limit: usize) -> anyhow::Result<Vec<RequestRow>> {
        match &self.cache {
            Some(c) => c.recent_requests(limit.clamp(1, 500)),
            None => Ok(Vec::new()),
        }
    }

    /// Whether semantic recall is active (embeddings loaded) vs FTS5 keyword.
    pub fn semantic(&self) -> bool {
        self.embedder.is_some()
    }
}

/// Distill each target page and store the result — the shared loop behind the
/// explicit verb and the background tick. Per-page fail-soft: one bad page
/// lands in `failed` and the loop continues.
async fn distill_targets(
    cache: &Cache,
    distiller: &dyn Distiller,
    targets: Vec<String>,
) -> anyhow::Result<(Vec<DistilledPage>, Vec<DistillFailure>)> {
    let mut distilled = Vec::new();
    let mut failed = Vec::new();
    for target in targets {
        let Some(row) = cache.get_page(&target)? else { continue };
        match distiller.distill_page(&row.page).await {
            Ok(d) => {
                cache.put_distillation(&target, &d, &row.page.content_hash)?;
                distilled.push(DistilledPage {
                    url:        target,
                    title:      row.page.title,
                    summary:    d.summary,
                    key_points: d.key_points,
                    entities:   d.entities,
                    tags:       d.tags,
                    model:      d.model,
                    backend:    d.backend.to_string(),
                    from_cache: false,
                });
            }
            Err(e) => failed.push(DistillFailure { url: target, error: e.to_string() }),
        }
    }
    Ok((distilled, failed))
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

    // ---- click + submit (Phase 13) ---------------------------------------

    /// Records every GET url and every general request — the wire-level
    /// assertions for the interaction verbs.
    struct Recording {
        body: Vec<u8>,
        /// Body served for POST `request`s (defaults to `body`).
        post_body: Vec<u8>,
        gets: std::sync::Mutex<Vec<String>>,
        reqs: std::sync::Mutex<Vec<HttpRequest>>,
    }
    impl Recording {
        fn new(body: &str) -> Arc<Self> {
            Arc::new(Self {
                body: body.as_bytes().to_vec(),
                post_body: body.as_bytes().to_vec(),
                gets: std::sync::Mutex::new(Vec::new()),
                reqs: std::sync::Mutex::new(Vec::new()),
            })
        }
        /// GETs serve `body`; POSTs serve `post_body` — a real interaction
        /// lands on a page that differs from its source.
        fn with_post_body(body: &str, post_body: &str) -> Arc<Self> {
            Arc::new(Self {
                body: body.as_bytes().to_vec(),
                post_body: post_body.as_bytes().to_vec(),
                gets: std::sync::Mutex::new(Vec::new()),
                reqs: std::sync::Mutex::new(Vec::new()),
            })
        }
        fn gets(&self) -> Vec<String> {
            self.gets.lock().unwrap().clone()
        }
        fn reqs(&self) -> Vec<HttpRequest> {
            self.reqs.lock().unwrap().clone()
        }
    }
    #[async_trait]
    impl Fetcher for Recording {
        async fn get(&self, url: &str) -> anyhow::Result<FetchResponse> {
            self.gets.lock().unwrap().push(url.to_string());
            Ok(FetchResponse {
                final_url: url.to_string(), status: 200, content_type: None,
                etag: None, last_modified: None, body: self.body.clone(),
                source: Source::Network,
            })
        }
        async fn request(&self, req: HttpRequest) -> anyhow::Result<FetchResponse> {
            let url = req.url.clone();
            self.reqs.lock().unwrap().push(req);
            Ok(FetchResponse {
                final_url: url, status: 200, content_type: None,
                etag: None, last_modified: None, body: self.post_body.clone(),
                source: Source::Network,
            })
        }
    }

    const SUBMIT_HTML: &str = r#"<html><head><title>F</title></head><body><main>
        <p><a href="/a">first</a> <a href="/b">second</a></p>
        <form action="/search?stale=1">
          <input type="text" name="q" value="">
          <button>Find</button>
        </form>
        <form action="/login" method="post">
          <input type="hidden" name="csrf" value="tok123">
          <input type="text" name="user" value="andre">
          <input type="password" name="pw">
          <button>Sign in</button>
        </form>
        </main></body></html>"#;

    #[test]
    fn element_selector_grammar_parses_and_rejects() {
        assert_eq!(parse_element("link:3").unwrap(), ElementSel::Link(3));
        assert_eq!(parse_element(" Form:1 ").unwrap(), ElementSel::Form(1));
        assert!(parse_element("button:1").is_err(), "unknown kind");
        assert!(parse_element("link:0").is_err(), "ordinals are 1-based");
        assert!(parse_element("link").is_err(), "missing ordinal");
        assert!(parse_element("link:x").is_err(), "non-numeric ordinal");
    }

    #[tokio::test]
    async fn click_link_by_ordinal_navigates_politely() {
        let f = Recording::new(SUBMIT_HTML);
        let e = engine(f.clone(), 3600);
        let report = e.click("https://e.test/f", "link:2").await.unwrap();
        assert_eq!(report.target_url, "https://e.test/b");
        assert_eq!(report.source_url, "https://e.test/f");
        assert_eq!(f.gets(), vec!["https://e.test/f", "https://e.test/b"], "source + one polite GET");

        let err = e.click("https://e.test/f", "link:9").await.unwrap_err().to_string();
        assert!(err.contains("no link #9"), "got: {err}");
        assert!(err.contains("2 links"), "tells the agent the registry size: {err}");
    }

    #[tokio::test]
    async fn submit_get_replaces_the_query_and_reuses_the_cache() {
        let f = Recording::new(SUBMIT_HTML);
        let e = engine(f.clone(), 3600);
        let report = e
            .submit("https://e.test/f", 1, &[("q".into(), "hello world".into())])
            .await
            .unwrap();
        assert_eq!(report.method, "get");
        assert!(!report.cached);
        assert_eq!(
            f.gets()[1],
            "https://e.test/search?q=hello+world",
            "form data replaces the stale action query"
        );

        // The same ask again: source page AND result are cache hits — the
        // repeated identical submission costs zero live requests.
        let report = e
            .submit("https://e.test/f", 1, &[("q".into(), "hello world".into())])
            .await
            .unwrap();
        assert!(report.cached);
        assert_eq!(f.gets().len(), 2, "no new live requests");
    }

    #[tokio::test]
    async fn submit_post_sends_urlencoded_once_and_never_caches_the_result() {
        let f = Recording::new(SUBMIT_HTML);
        let e = engine(f.clone(), 3600);
        let report = e
            .submit("https://e.test/f", 2, &[("pw".into(), "s3cret".into())])
            .await
            .unwrap();
        assert_eq!(report.method, "post");
        assert_eq!(report.status, Some(200));
        assert!(!report.cached);

        let reqs = f.reqs();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].url, "https://e.test/login");
        assert_eq!(reqs[0].headers[0].1, "application/x-www-form-urlencoded");
        assert!(reqs[0].robots, "form POST is robots-gated");
        let body = String::from_utf8(reqs[0].body.clone().unwrap()).unwrap();
        assert_eq!(body, "csrf=tok123&user=andre&pw=s3cret", "hidden state verbatim, field order kept");

        // Only the source page is in the cache — the POST result never lands.
        assert_eq!(e.stats().unwrap().pages, 1);

        // The report redacts the password; the wire carried the real value.
        let pw = report.sent.iter().find(|s| s.name == "pw").unwrap();
        assert_eq!(pw.value, "•••");
        assert!(body.contains("pw=s3cret"));
    }

    #[tokio::test]
    async fn submit_rejects_an_unknown_field_name() {
        let f = Recording::new(SUBMIT_HTML);
        let e = engine(f.clone(), 3600);
        let err = e
            .submit("https://e.test/f", 1, &[("qq".into(), "typo".into())])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no field named \"qq\""), "got: {err}");
        assert!(err.contains("\"q\""), "lists the real fields: {err}");
        assert!(f.reqs().is_empty() && f.gets().len() == 1, "nothing was submitted");
    }

    #[tokio::test]
    async fn clicking_a_form_submits_it_with_current_values() {
        let f = Recording::new(SUBMIT_HTML);
        let e = engine(f.clone(), 3600);
        let report = e.click("https://e.test/f", "form:2").await.unwrap();
        assert_eq!(report.target_url, "https://e.test/login");
        assert_eq!(report.status, Some(200));
        let body = String::from_utf8(f.reqs()[0].body.clone().unwrap()).unwrap();
        assert_eq!(body, "csrf=tok123&user=andre&pw=", "current values, hidden verbatim");
    }

    /// A POST-result SERP: 3 result links + Previous/Next POST pagination
    /// forms — the shape that capped interaction depth at one hop in the
    /// field (apex1, 2026-07-26).
    const RESULT_HTML: &str = r#"<html><head><title>R</title></head><body><main>
        <p><a href="/r1">res one</a> <a href="/r2">res two</a> <a href="/r3">res three</a></p>
        <form action="/page" method="post">
          <input type="hidden" name="page" value="0"><button>Previous</button>
        </form>
        <form action="/page" method="post">
          <input type="hidden" name="page" value="2"><button>Next Page</button>
        </form>
        </main></body></html>"#;

    #[tokio::test]
    async fn post_result_is_addressable_via_handle_and_source_registry_untouched() {
        let f = Recording::with_post_body(SUBMIT_HTML, RESULT_HTML);
        let e = engine(f.clone(), 3600);

        // POST form #2 → the report mints a working-memory handle.
        let report = e.submit("https://e.test/f", 2, &[]).await.unwrap();
        assert_eq!(report.method, "post");
        let handle = report.handle.expect("a POST result carries a handle");
        assert!(handle.starts_with("result:"), "opaque handle: {handle}");

        // The handle resolves the RESULT page's registry (3 links, 2 forms)…
        let dom = e.dom(&handle, false).await.unwrap();
        assert_eq!(dom.links.len(), 3, "the result page's own links");
        assert_eq!(dom.forms.len(), 2, "…and its own forms");

        // …while the source URL's registry is untouched (the collision the
        // naive fix would cause: final_url == source URL).
        let src = e.dom("https://e.test/f", false).await.unwrap();
        assert_eq!(src.links.len(), 2, "source registry unchanged");
        assert_eq!(src.forms.len(), 2, "source registry unchanged");

        // Interacting THROUGH the handle: submit the result's "Next Page"
        // form — the second deliberate POST, minting a fresh handle.
        let next = e.submit(&handle, 2, &[]).await.unwrap();
        assert!(next.handle.is_some(), "pagination keeps yielding handles");
        assert_eq!(f.reqs().len(), 2, "two deliberate POSTs, nothing replayed");

        // Clicking a result link off the handle is a normal polite GET —
        // URL-addressable, so no handle.
        let click = e.click(&handle, "link:1").await.unwrap();
        assert_eq!(click.target_url, "https://e.test/r1");
        assert!(click.handle.is_none(), "a followed link needs no handle");

        // An unknown/expired handle fails honestly, with the way out.
        let err = e.dom("result:deadbeef00000000", false).await.unwrap_err().to_string();
        assert!(err.contains("result handle"), "got: {err}");
        assert!(err.contains("GET URL"), "points at the workaround: {err}");
    }

    // ---- dom + snapshots (Phase 12) --------------------------------------

    const FORM_HTML: &str = r#"<html><head><title>S</title></head><body><main>
        <p>Find <a href="/a">first</a> and <a href="/b">second</a>.</p>
        <form action="/search"><input type="search" name="q"><button>Go</button></form>
        </main></body></html>"#;

    #[tokio::test]
    async fn fetch_stores_a_snapshot_and_dom_returns_the_registry() {
        let f = Counting::new(FORM_HTML);
        let e = engine(f.clone(), 3600);
        e.fetch("https://e.test/", false).await.unwrap();

        let view = e.dom("https://e.test/", false).await.unwrap();
        assert!(view.from_cache, "dom is cache-first — no second live fetch");
        assert_eq!(f.calls(), 1);
        assert!(view.snapshot, "the fetch left a resolvable snapshot");
        assert_eq!(view.forms.len(), 1);
        assert_eq!(view.forms[0].action, "https://e.test/search");
        assert_eq!(view.forms[0].fields[0].name, "q");
        let idx: Vec<usize> = view.links.iter().map(|l| l.idx).collect();
        assert_eq!(idx, vec![1, 2], "links carry stable 1-based ordinals");
        assert_eq!(view.links[0].url, "https://e.test/a");
    }

    #[tokio::test]
    async fn dom_fetches_an_uncached_url_and_reports_expired_snapshots() {
        let f = Counting::new(FORM_HTML);
        let cache = Arc::new(Cache::open_in_memory().unwrap());
        let e = Engine::with_parts(f.clone(), Box::new(DuckDuckGo), Some(cache.clone()), 5, 3600);

        let view = e.dom("https://e.test/", false).await.unwrap();
        assert!(!view.from_cache, "uncached URL is fetched (read-through)");
        assert_eq!(f.calls(), 1);
        assert!(view.snapshot);

        // Expire the snapshot: the registry still serves from the cached page,
        // but honestly reports that an interaction would need a re-fetch.
        cache.gc_snapshots(0).unwrap();
        let view = e.dom("https://e.test/", false).await.unwrap();
        assert!(view.from_cache);
        assert!(!view.snapshot, "expired snapshot reported as absent");
        assert_eq!(f.calls(), 1, "reporting it costs no live request");
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
            forms: vec![],
            salvaged: false,
            js_required: false,
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

    // ---- distillation (knowledge hub) -------------------------------------

    use crate::curate::{Distillation, Distiller};

    /// A canned distiller that counts LLM calls and can fail on demand.
    struct MockDistiller {
        calls: AtomicUsize,
        fail:  bool,
    }
    impl MockDistiller {
        fn new() -> Arc<Self> {
            Arc::new(Self { calls: AtomicUsize::new(0), fail: false })
        }
        fn failing() -> Arc<Self> {
            Arc::new(Self { calls: AtomicUsize::new(0), fail: true })
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }
    #[async_trait]
    impl Distiller for MockDistiller {
        async fn distill_page(&self, page: &Page) -> anyhow::Result<Distillation> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                anyhow::bail!("model unreachable");
            }
            Ok(Distillation {
                summary:    format!("Distilled: {}", page.title.as_deref().unwrap_or("?")),
                key_points: vec!["a key point".into()],
                entities:   vec!["Entity".into()],
                tags:       vec!["tag1".into(), "tag2".into()],
                model:      "mock".into(),
                backend:    "ollama",
            })
        }
    }

    #[tokio::test]
    async fn distill_url_fetches_stores_and_upgrades_recall() {
        let f = Counting::new(HTML);
        let mock = MockDistiller::new();
        let e = engine(f, 3600).with_distiller(Some(mock.clone()));

        let report = e.distill(Some("https://e.test/"), None).await.unwrap();
        assert_eq!(report.distilled.len(), 1);
        assert_eq!(report.failed.len(), 0);
        assert_eq!(report.remaining, 0);
        assert!(!report.distilled[0].from_cache);
        assert_eq!(report.distilled[0].summary, "Distilled: T");
        assert_eq!(mock.calls(), 1, "uncached URL was fetched then distilled");

        // Recall now serves the curated summary + tags, keyword-findable via
        // distilled terms only ("tag1" is nowhere in the page body).
        let hits = e.recall("tag1", Some(5)).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].distilled);
        assert_eq!(hits[0].snippet, "Distilled: T");
        assert_eq!(hits[0].tags, vec!["tag1", "tag2"]);
    }

    #[tokio::test]
    async fn redistilling_unchanged_page_spends_no_llm_call() {
        let f = Counting::new(HTML);
        let mock = MockDistiller::new();
        let e = engine(f, 3600).with_distiller(Some(mock.clone()));
        e.distill(Some("https://e.test/"), None).await.unwrap();
        let report = e.distill(Some("https://e.test/"), None).await.unwrap();
        assert_eq!(mock.calls(), 1, "second ask on unchanged content is free");
        assert!(report.distilled[0].from_cache);
        assert_eq!(report.distilled[0].backend, "cache");
    }

    #[tokio::test]
    async fn sweep_distills_only_pending_pages_and_reports_failures() {
        let f = Counting::new(HTML);
        let mock = MockDistiller::new();
        let e = engine(f, 3600).with_distiller(Some(mock.clone()));
        e.fetch("https://e.test/a", false).await.unwrap();
        e.fetch("https://e.test/b", false).await.unwrap();

        let report = e.distill(None, None).await.unwrap();
        assert_eq!(report.distilled.len(), 2);
        assert_eq!(report.remaining, 0);
        assert_eq!(mock.calls(), 2);

        // Nothing pending → an empty sweep, zero LLM calls.
        let report = e.distill(None, None).await.unwrap();
        assert!(report.distilled.is_empty());
        assert_eq!(mock.calls(), 2);

        // A failing backend reports per-page failures, remaining still pending.
        let f2 = Counting::new(HTML);
        let e2 = engine(f2, 3600).with_distiller(Some(MockDistiller::failing()));
        e2.fetch("https://e.test/c", false).await.unwrap();
        let report = e2.distill(None, None).await.unwrap();
        assert_eq!(report.failed.len(), 1);
        assert!(report.failed[0].error.contains("model unreachable"));
        assert_eq!(report.remaining, 1, "failed page stays pending");
    }

    #[tokio::test]
    async fn distill_without_a_distiller_errors_honestly() {
        let e = engine(Counting::new(HTML), 3600);
        let err = e.distill(None, None).await.unwrap_err().to_string();
        assert!(err.contains("curation disabled"), "got: {err}");
        assert!(!e.curation());
    }

    // ---- auto-distillation (the background tick) ---------------------------

    #[tokio::test]
    async fn auto_tick_is_a_noop_when_auto_is_off() {
        // A main distiller alone does NOT enable the background sweep.
        let e = engine(Counting::new(HTML), 3600).with_distiller(Some(MockDistiller::new()));
        e.fetch("https://e.test/", false).await.unwrap();
        assert!(!e.auto_curation());
        assert!(e.auto_distill_tick().await.unwrap().is_none());
        assert_eq!(e.stats().unwrap().distilled, 0);
    }

    #[tokio::test]
    async fn auto_tick_sweeps_pending_pages_then_goes_quiet() {
        let mock = MockDistiller::new();
        let e = engine(Counting::new(HTML), 3600).with_auto_distiller(Some(mock.clone()), 0);
        e.fetch("https://e.test/a", false).await.unwrap();
        e.fetch("https://e.test/b", false).await.unwrap();

        let report = e.auto_distill_tick().await.unwrap().expect("swept");
        assert_eq!(report.distilled.len(), 2);
        assert_eq!(report.remaining, 0);
        assert_eq!(mock.calls(), 2);

        // Nothing pending → quiet tick, zero LLM calls.
        assert!(e.auto_distill_tick().await.unwrap().is_none());
        assert_eq!(mock.calls(), 2);
    }

    #[tokio::test]
    async fn auto_tick_honors_the_rolling_budget() {
        let mock = MockDistiller::new();
        // Cap = 1 distillation per 24 h.
        let e = engine(Counting::new(HTML), 3600).with_auto_distiller(Some(mock.clone()), 1);
        e.fetch("https://e.test/a", false).await.unwrap();
        e.fetch("https://e.test/b", false).await.unwrap();

        // First tick: batch is clamped to the remaining budget (1 of 2 pages).
        let report = e.auto_distill_tick().await.unwrap().expect("swept one");
        assert_eq!(report.distilled.len(), 1);
        assert_eq!(report.remaining, 1);

        // Budget spent → the next tick pauses even with a page still pending.
        assert!(e.auto_distill_tick().await.unwrap().is_none());
        assert_eq!(mock.calls(), 1, "cap held");
    }

    // ---- decay & GC (Phase 6) -------------------------------------------

    use chrono::Duration;

    /// Like `recall_engine` but hands back the cache so a test can backdate pages.
    fn engine_and_cache(pages: &[(&str, &str)]) -> (Engine, Arc<Cache>) {
        let cache = Arc::new(Cache::open_in_memory().unwrap());
        for (url, body) in pages {
            cache.put_page(&mkpage(url, body), None, None, false).unwrap();
        }
        let engine = Engine::with_parts(Counting::new(""), Box::new(DuckDuckGo), Some(cache.clone()), 5, 3600);
        (engine, cache)
    }

    #[test]
    fn gc_prunes_stale_unpinned_keeps_pinned_and_fresh() {
        let (e, cache) = engine_and_cache(&[
            ("https://e.test/stale", "old unused page"),
            ("https://e.test/pinned", "old but pinned"),
            ("https://e.test/fresh", "recently used"),
        ]);
        let old = (Utc::now() - Duration::days(60)).to_rfc3339();
        cache.set_timestamps("https://e.test/stale", &old, &old);
        cache.set_timestamps("https://e.test/pinned", &old, &old);
        cache.set_pinned("https://e.test/pinned", true).unwrap();
        // half-life 1 day → 60 days of disuse decays the stale page to ~0.
        let e = e.with_gc_params(86_400, 0.15, 3_600);
        let pruned = e.gc().unwrap();
        assert_eq!(pruned, 1, "only the stale, unpinned page is pruned");
        assert!(cache.get_page("https://e.test/stale").unwrap().is_none());
        assert!(cache.get_page("https://e.test/pinned").unwrap().is_some(), "pinned survives decay");
        assert!(cache.get_page("https://e.test/fresh").unwrap().is_some(), "fresh survives");
    }

    #[tokio::test]
    async fn recall_decay_sinks_stale_pages_below_fresh() {
        // Identical content → equal relevance; only recency should separate them.
        let (e, cache) = engine_and_cache(&[
            ("https://e.test/old", "rust programming language"),
            ("https://e.test/new", "rust programming language"),
        ]);
        let old = (Utc::now() - Duration::days(30)).to_rfc3339();
        cache.set_timestamps("https://e.test/old", &old, &old);
        let e = e.with_gc_params(86_400, 0.15, 3_600); // sharp 1-day half-life
        let hits = e.recall("rust", Some(2)).await.unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].url, "https://e.test/new", "the fresher page ranks first");
    }
}
