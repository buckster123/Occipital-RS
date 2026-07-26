//! The read-through knowledge cache — SQLite store of fetched pages + search
//! result sets. Every `web_fetch`/`web_search` consults this first; only a miss
//! or a stale entry hits the live web (a conditional GET when possible). This is
//! the biggest politeness multiplier — a re-asked query costs zero requests.
//!
//! Decay/forgetting (the `salience` column + a GC) lands in Phase 6; semantic
//! recall over the store (FTS5 + embeddings) in Phase 5. Phase 4 is the store
//! and the read-through itself. Freshness *policy* (the TTL) lives in the
//! `engine`; the cache only records `fetched_at`.

use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::curate::Distillation;
use crate::extract::{Form, Link, Page};
use crate::providers::SearchResult;

/// Cache size counters (for `stats`).
#[derive(Debug, Clone, Serialize)]
pub struct CacheStats {
    pub pages:      i64,
    pub pinned:     i64,
    pub embeddings: i64,
    pub searches:   i64,
    pub distilled:  i64,
    pub snapshots:  i64,
    pub requests:   i64,
}

/// A cached page plus the metadata the read-through needs (validators for a
/// conditional refresh, `fetched_at` for freshness, `pinned` to survive decay).
#[derive(Debug, Clone)]
pub struct PageRow {
    pub page:          Page,
    pub etag:          Option<String>,
    pub last_modified: Option<String>,
    pub fetched_at:    DateTime<Utc>,
    pub pinned:        bool,
}

/// A stored distillation (curated knowledge) plus the page hash it distilled —
/// a hash mismatch against the current page marks it stale (re-distill).
#[derive(Debug, Clone, Serialize)]
pub struct DistillRow {
    pub summary:      String,
    pub key_points:   Vec<String>,
    pub entities:     Vec<String>,
    pub tags:         Vec<String>,
    pub content_hash: String,
    pub model:        Option<String>,
    pub distilled_at: String,
}

/// One row of the request log (what we actually sent, and what it cost).
#[derive(Debug, Clone, Serialize)]
pub struct RequestRow {
    pub at:          String,
    pub method:      String,
    pub url:         String,
    pub status:      Option<u16>,
    pub wait_ms:     u64,
    pub duration_ms: u64,
    pub error:       Option<String>,
}

/// Lightweight per-page metadata for decay ranking + GC (no body).
#[derive(Debug, Clone)]
pub struct PageMeta {
    pub url:         String,
    pub fetched_at:  DateTime<Utc>,
    pub last_access: DateTime<Utc>,
    pub salience:    f32,
    pub pinned:      bool,
}

pub struct Cache {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS pages (
    url           TEXT PRIMARY KEY,
    title         TEXT,
    byline        TEXT,
    markdown      TEXT NOT NULL,
    links         TEXT NOT NULL,            -- JSON array of {text,url}
    forms         TEXT NOT NULL DEFAULT '[]', -- JSON array of Form (Phase 12)
    salvaged      INTEGER NOT NULL DEFAULT 0, -- content mined from embedded data (Phase 14)
    js_required   INTEGER NOT NULL DEFAULT 0, -- client-only page, nothing recoverable
    content_hash  TEXT NOT NULL,
    etag          TEXT,
    last_modified TEXT,
    fetched_at    TEXT NOT NULL,            -- RFC3339
    last_access   TEXT NOT NULL,
    access_count  INTEGER NOT NULL DEFAULT 0,
    salience      REAL    NOT NULL DEFAULT 1.0,  -- decays in Phase 6
    pinned        INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS searches (
    key        TEXT PRIMARY KEY,            -- provider|limit|normalized-query
    query      TEXT NOT NULL,
    results    TEXT NOT NULL,               -- JSON array of SearchResult
    fetched_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS embeddings (
    url TEXT PRIMARY KEY,
    vec BLOB NOT NULL                       -- little-endian f32s (Micro+ only)
);
CREATE VIRTUAL TABLE IF NOT EXISTS pages_fts USING fts5(url UNINDEXED, title, markdown);
CREATE TABLE IF NOT EXISTS distillations (
    url          TEXT PRIMARY KEY,          -- FK to pages (cascade in delete_page)
    summary      TEXT NOT NULL,
    key_points   TEXT NOT NULL,             -- JSON array of strings
    entities     TEXT NOT NULL,             -- JSON array of strings
    tags         TEXT NOT NULL,             -- JSON array of strings
    content_hash TEXT NOT NULL,             -- page hash distilled (stale detection)
    model        TEXT,                      -- provenance
    distilled_at TEXT NOT NULL              -- RFC3339
);
CREATE VIRTUAL TABLE IF NOT EXISTS distill_fts USING fts5(url UNINDEXED, summary, terms);
CREATE TABLE IF NOT EXISTS requests (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    at          TEXT    NOT NULL,        -- RFC3339
    method      TEXT    NOT NULL,
    url         TEXT    NOT NULL,
    status      INTEGER,                 -- NULL when blocked or errored
    wait_ms     INTEGER NOT NULL,        -- politeness budget wait
    duration_ms INTEGER NOT NULL,
    error       TEXT
);
CREATE TABLE IF NOT EXISTS snapshots (
    url        TEXT PRIMARY KEY,            -- FK to pages (cascade in delete_page)
    html       TEXT NOT NULL,               -- raw fetched HTML (body-cap bounded)
    fetched_at TEXT NOT NULL                -- RFC3339; TTL-pruned working memory
);
"#;

impl Cache {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> anyhow::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> anyhow::Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    // ---- pages -----------------------------------------------------------

    pub fn get_page(&self, url: &str) -> anyhow::Result<Option<PageRow>> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT title, byline, markdown, links, forms, salvaged, js_required, \
                        content_hash, etag, last_modified, fetched_at, pinned, md_alt, src_fmt \
                 FROM pages WHERE url = ?1",
                params![url],
                |r| {
                    let links_json: String = r.get(3)?;
                    let forms_json: String = r.get(4)?;
                    let fetched_at: String = r.get(10)?;
                    Ok((
                        r.get::<_, Option<String>>(0)?, // title
                        r.get::<_, Option<String>>(1)?, // byline
                        r.get::<_, String>(2)?,         // markdown
                        links_json,
                        forms_json,
                        r.get::<_, i64>(5)? != 0,        // salvaged
                        r.get::<_, i64>(6)? != 0,        // js_required
                        r.get::<_, String>(7)?,         // content_hash
                        r.get::<_, Option<String>>(8)?, // etag
                        r.get::<_, Option<String>>(9)?, // last_modified
                        fetched_at,
                        r.get::<_, i64>(11)? != 0,       // pinned
                        r.get::<_, Option<String>>(12)?, // md_alt
                        r.get::<_, Option<String>>(13)?, // src_fmt
                    ))
                },
            )
            .optional()?;

        let Some((title, byline, markdown, links_json, forms_json, salvaged, js_required, content_hash, etag, last_modified, fetched_at, pinned, md_alt, src_fmt)) = row
        else {
            return Ok(None);
        };
        let links: Vec<Link> = serde_json::from_str(&links_json).unwrap_or_default();
        let mut forms: Vec<Form> = serde_json::from_str(&forms_json).unwrap_or_default();
        // `submittable` is derivable from data already in the row — recompute
        // rather than trust the stored value, so rows written before the flag
        // existed (serde default: true) are truthful instead of
        // stale-optimistic. A pre-flight flag that says "yes" and then eats a
        // refusal is worse than no flag (apex1 seam report, 2026-07-26).
        for f in &mut forms {
            f.submittable = crate::extract::form_is_submittable(&f.method, &f.fields);
        }
        let fetched_at = parse_ts(&fetched_at)?;
        Ok(Some(PageRow {
            page: Page {
                url: url.to_string(),
                title,
                byline,
                markdown,
                links,
                forms,
                salvaged,
                js_required,
                content_hash,
                markdown_alternate: md_alt,
                source_format: src_fmt,
            },
            etag,
            last_modified,
            fetched_at,
            pinned,
        }))
    }

    /// Insert or refresh a page. On conflict the content/validators/`fetched_at`
    /// update but `access_count` + `salience` are preserved (a refresh is not a
    /// new memory). `pinned` is set explicitly by the caller.
    pub fn put_page(
        &self,
        page: &Page,
        etag: Option<&str>,
        last_modified: Option<&str>,
        pinned: bool,
    ) -> anyhow::Result<()> {
        let now = Utc::now().to_rfc3339();
        let links = serde_json::to_string(&page.links)?;
        let forms = serde_json::to_string(&page.forms)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO pages \
               (url, title, byline, markdown, links, forms, salvaged, js_required, \
                content_hash, etag, last_modified, \
                fetched_at, last_access, access_count, salience, pinned, md_alt, src_fmt) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?12,0,1.0,?13,?14,?15) \
             ON CONFLICT(url) DO UPDATE SET \
               title=excluded.title, byline=excluded.byline, markdown=excluded.markdown, \
               links=excluded.links, forms=excluded.forms, salvaged=excluded.salvaged, \
               js_required=excluded.js_required, content_hash=excluded.content_hash, \
               etag=excluded.etag, last_modified=excluded.last_modified, \
               fetched_at=excluded.fetched_at, last_access=excluded.last_access, \
               pinned=excluded.pinned, md_alt=excluded.md_alt, src_fmt=excluded.src_fmt",
            params![
                page.url, page.title, page.byline, page.markdown, links, forms,
                page.salvaged as i64, page.js_required as i64,
                page.content_hash, etag, last_modified, now, pinned as i64,
                page.markdown_alternate, page.source_format
            ],
        )?;
        // Keep the FTS keyword index in sync (delete-then-insert; FTS5 has no upsert).
        conn.execute("DELETE FROM pages_fts WHERE url=?1", params![page.url])?;
        conn.execute(
            "INSERT INTO pages_fts (url, title, markdown) VALUES (?1,?2,?3)",
            params![page.url, page.title, page.markdown],
        )?;
        Ok(())
    }

    /// Store a page's embedding vector (Micro+). Replaces any prior vector.
    pub fn put_embedding(&self, url: &str, vec: &[f32]) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO embeddings (url, vec) VALUES (?1, ?2)",
            params![url, vec_to_blob(vec)],
        )?;
        Ok(())
    }

    /// Every stored `(url, vector)` — brute-force cosine search loads these. Fine
    /// at cache scale; an ANN index (sqlite-vec) is the scale refinement.
    pub fn all_embeddings(&self) -> anyhow::Result<Vec<(String, Vec<f32>)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT url, vec FROM embeddings")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, blob_to_vec(&r.get::<_, Vec<u8>>(1)?)))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// FTS5 keyword search over cached pages **and their distillations** →
    /// matching urls, best-ranked first. Raw-body hits lead; pages found only
    /// via distilled terms (summary/tags/entities) are appended — so curation
    /// widens keyword recall (the Nano win: no embeddings needed).
    pub fn keyword_search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<String>> {
        let m = fts_query(query);
        if m.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT url FROM pages_fts WHERE pages_fts MATCH ?1 ORDER BY rank LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![m, limit as i64], |r| r.get::<_, String>(0))?;
        let mut urls: Vec<String> = rows.collect::<Result<Vec<_>, _>>()?;

        let mut stmt = conn.prepare(
            "SELECT url FROM distill_fts WHERE distill_fts MATCH ?1 ORDER BY rank LIMIT ?2",
        )?;
        let distilled = stmt.query_map(params![m, limit as i64], |r| r.get::<_, String>(0))?;
        for url in distilled {
            let url = url?;
            if urls.len() >= limit {
                break;
            }
            if !urls.contains(&url) {
                urls.push(url);
            }
        }
        urls.truncate(limit);
        Ok(urls)
    }

    // ---- distillations (LLM curation) --------------------------------------

    /// Store (or replace) a page's distillation, recording the page hash it was
    /// distilled from, and index its summary + tags/entities/key-points for
    /// keyword recall.
    pub fn put_distillation(
        &self,
        url: &str,
        d: &Distillation,
        content_hash: &str,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO distillations \
               (url, summary, key_points, entities, tags, content_hash, model, distilled_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                url,
                d.summary,
                serde_json::to_string(&d.key_points)?,
                serde_json::to_string(&d.entities)?,
                serde_json::to_string(&d.tags)?,
                content_hash,
                d.model,
                Utc::now().to_rfc3339(),
            ],
        )?;
        // terms: everything findable that isn't the summary prose.
        let terms = d
            .tags
            .iter()
            .chain(d.entities.iter())
            .chain(d.key_points.iter())
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" ");
        conn.execute("DELETE FROM distill_fts WHERE url=?1", params![url])?;
        conn.execute(
            "INSERT INTO distill_fts (url, summary, terms) VALUES (?1,?2,?3)",
            params![url, d.summary, terms],
        )?;
        Ok(())
    }

    pub fn get_distillation(&self, url: &str) -> anyhow::Result<Option<DistillRow>> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT summary, key_points, entities, tags, content_hash, model, distilled_at \
                 FROM distillations WHERE url=?1",
                params![url],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, Option<String>>(5)?,
                        r.get::<_, String>(6)?,
                    ))
                },
            )
            .optional()?;
        let Some((summary, key_points, entities, tags, content_hash, model, distilled_at)) = row
        else {
            return Ok(None);
        };
        let list = |s: &str| serde_json::from_str::<Vec<String>>(s).unwrap_or_default();
        Ok(Some(DistillRow {
            summary,
            key_points: list(&key_points),
            entities: list(&entities),
            tags: list(&tags),
            content_hash,
            model,
            distilled_at,
        }))
    }

    /// Pages needing distillation: never distilled, or the page content changed
    /// since (hash mismatch). Newest fetches first — curate fresh reading first.
    pub fn undistilled_urls(&self, limit: usize) -> anyhow::Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT p.url FROM pages p LEFT JOIN distillations d ON p.url = d.url \
             WHERE d.url IS NULL OR d.content_hash != p.content_hash \
             ORDER BY p.fetched_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Distillations performed since `cutoff` (by `distilled_at`) — the
    /// auto-distill budget counter. RFC3339 UTC strings compare lexically.
    pub fn distilled_since(&self, cutoff: DateTime<Utc>) -> anyhow::Result<usize> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM distillations WHERE distilled_at > ?1",
            params![cutoff.to_rfc3339()],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    /// How many cached pages still need distillation (see `undistilled_urls`).
    pub fn undistilled_count(&self) -> anyhow::Result<usize> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pages p LEFT JOIN distillations d ON p.url = d.url \
             WHERE d.url IS NULL OR d.content_hash != p.content_hash",
            [],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    /// Record a cache hit: bump `last_access` + `access_count`, and reinforce
    /// salience (capped) — a re-read page earns standing against decay (ACT-R).
    pub fn touch_page(&self, url: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE pages SET last_access=?2, access_count=access_count+1, \
                    salience=MIN(1.0, salience+0.05) WHERE url=?1",
            params![url, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// A `304 Not Modified` refresh: the content is still current, so slide
    /// `fetched_at` forward (and count the access) without rewriting the body.
    pub fn mark_fresh(&self, url: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE pages SET fetched_at=?2, last_access=?2, access_count=access_count+1, \
                    salience=MIN(1.0, salience+0.05) WHERE url=?1",
            params![url, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// Per-page metadata for decay ranking + GC.
    pub fn all_page_meta(&self) -> anyhow::Result<Vec<PageMeta>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT url, fetched_at, last_access, salience, pinned FROM pages",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, f64>(3)? as f32,
                r.get::<_, i64>(4)? != 0,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (url, fetched_at, last_access, salience, pinned) = row?;
            out.push(PageMeta {
                url,
                fetched_at: parse_ts(&fetched_at)?,
                last_access: parse_ts(&last_access)?,
                salience,
                pinned,
            });
        }
        Ok(out)
    }

    pub fn delete_page(&self, url: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock().unwrap();
        let removed = conn.execute("DELETE FROM pages WHERE url=?1", params![url])? > 0;
        conn.execute("DELETE FROM pages_fts WHERE url=?1", params![url])?;
        conn.execute("DELETE FROM embeddings WHERE url=?1", params![url])?;
        conn.execute("DELETE FROM distillations WHERE url=?1", params![url])?;
        conn.execute("DELETE FROM distill_fts WHERE url=?1", params![url])?;
        conn.execute("DELETE FROM snapshots WHERE url=?1", params![url])?;
        Ok(removed)
    }

    // ---- snapshots (Phase 12: interaction working memory) -----------------

    /// Store (or replace) a page's raw-HTML snapshot. Working memory for the
    /// interaction verbs — never recalled, TTL-pruned by [`gc_snapshots`](Self::gc_snapshots).
    pub fn put_snapshot(&self, url: &str, html: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO snapshots (url, html, fetched_at) VALUES (?1,?2,?3)",
            params![url, html, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// The stored snapshot + its capture time, if one exists. Freshness policy
    /// (the TTL) is the engine's call, mirroring `fetched_at` on pages.
    pub fn get_snapshot(&self, url: &str) -> anyhow::Result<Option<(String, DateTime<Utc>)>> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT html, fetched_at FROM snapshots WHERE url=?1",
                params![url],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?;
        let Some((html, fetched_at)) = row else { return Ok(None) };
        Ok(Some((html, parse_ts(&fetched_at)?)))
    }

    // ---- request log (Phase 16: the honest trail) --------------------------

    /// Append one request record, keeping only the newest `max` rows. `max = 0`
    /// disables the log entirely.
    pub fn log_request(&self, e: &crate::fetch::RequestEntry, max: usize) -> anyhow::Result<()> {
        if max == 0 {
            return Ok(());
        }
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO requests (at, method, url, status, wait_ms, duration_ms, error) \
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                e.at.to_rfc3339(), e.method, e.url, e.status,
                e.wait_ms as i64, e.duration_ms as i64, e.error
            ],
        )?;
        // Ids are monotonic, so an indexed range delete bounds the table cheaply.
        let newest = conn.last_insert_rowid();
        conn.execute("DELETE FROM requests WHERE id <= ?1", params![newest - max as i64])?;
        Ok(())
    }

    /// The most recent requests, newest first.
    pub fn recent_requests(&self, limit: usize) -> anyhow::Result<Vec<RequestRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT at, method, url, status, wait_ms, duration_ms, error \
             FROM requests ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok(RequestRow {
                at:          r.get(0)?,
                method:      r.get(1)?,
                url:         r.get(2)?,
                status:      r.get::<_, Option<i64>>(3)?.map(|s| s as u16),
                wait_ms:     r.get::<_, i64>(4)? as u64,
                duration_ms: r.get::<_, i64>(5)? as u64,
                error:       r.get(6)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Prune snapshots older than `ttl_secs`. Returns the number removed.
    pub fn gc_snapshots(&self, ttl_secs: u64) -> anyhow::Result<usize> {
        let cutoff = (Utc::now() - chrono::Duration::seconds(ttl_secs as i64)).to_rfc3339();
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("DELETE FROM snapshots WHERE fetched_at < ?1", params![cutoff])?)
    }

    pub fn set_pinned(&self, url: &str, pinned: bool) -> anyhow::Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("UPDATE pages SET pinned=?2 WHERE url=?1", params![url, pinned as i64])? > 0)
    }

    // ---- searches --------------------------------------------------------

    pub fn get_search(&self, key: &str) -> anyhow::Result<Option<(Vec<SearchResult>, DateTime<Utc>)>> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT results, fetched_at FROM searches WHERE key=?1",
                params![key],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?;
        let Some((results_json, fetched_at)) = row else { return Ok(None) };
        let results: Vec<SearchResult> = serde_json::from_str(&results_json).unwrap_or_default();
        Ok(Some((results, parse_ts(&fetched_at)?)))
    }

    /// Size counters across the store.
    pub fn stats(&self) -> CacheStats {
        let conn = self.conn.lock().unwrap();
        let count = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap_or(0);
        CacheStats {
            pages:      count("SELECT COUNT(*) FROM pages"),
            pinned:     count("SELECT COUNT(*) FROM pages WHERE pinned=1"),
            embeddings: count("SELECT COUNT(*) FROM embeddings"),
            searches:   count("SELECT COUNT(*) FROM searches"),
            distilled:  count("SELECT COUNT(*) FROM distillations"),
            snapshots:  count("SELECT COUNT(*) FROM snapshots"),
            requests:   count("SELECT COUNT(*) FROM requests"),
        }
    }

    pub fn put_search(&self, key: &str, query: &str, results: &[SearchResult]) -> anyhow::Result<()> {
        let json = serde_json::to_string(results)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO searches (key, query, results, fetched_at) VALUES (?1,?2,?3,?4)",
            params![key, query, json, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub fn page_count(&self) -> i64 {
        self.conn.lock().unwrap()
            .query_row("SELECT COUNT(*) FROM pages", [], |r| r.get(0)).unwrap_or(0)
    }

    /// Backdate a page's timestamps (RFC3339) — lets decay/GC tests age a page
    /// without sleeping.
    #[cfg(test)]
    pub fn set_timestamps(&self, url: &str, fetched_at: &str, last_access: &str) {
        self.conn.lock().unwrap()
            .execute(
                "UPDATE pages SET fetched_at=?2, last_access=?3 WHERE url=?1",
                params![url, fetched_at, last_access],
            )
            .unwrap();
    }
}

/// Additive column migrations for DBs created by earlier schema versions —
/// `CREATE TABLE IF NOT EXISTS` never alters an existing table.
fn migrate(conn: &Connection) -> anyhow::Result<()> {
    const ADDED: &[(&str, &str)] = &[
        ("forms", "TEXT NOT NULL DEFAULT '[]'"),          // Phase 12
        ("salvaged", "INTEGER NOT NULL DEFAULT 0"),       // Phase 14
        ("js_required", "INTEGER NOT NULL DEFAULT 0"),    // Phase 14
        ("md_alt", "TEXT"),                               // field pass round 4
        ("src_fmt", "TEXT"),                              // field pass round 6
    ];
    for (col, ddl) in ADDED {
        let present: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('pages') WHERE name=?1",
            params![col],
            |r| r.get(0),
        )?;
        if present == 0 {
            conn.execute(&format!("ALTER TABLE pages ADD COLUMN {col} {ddl}"), [])?;
        }
    }
    Ok(())
}

/// Adapts the cache into the fetcher's [`RequestSink`](crate::fetch::RequestSink)
/// seam: `fetch` stays the bottom layer and never learns about SQLite.
pub struct CacheLog {
    cache: std::sync::Arc<Cache>,
    max:   usize,
}

impl CacheLog {
    pub fn new(cache: std::sync::Arc<Cache>, max: usize) -> Self {
        Self { cache, max }
    }
}

impl crate::fetch::RequestSink for CacheLog {
    fn record(&self, entry: crate::fetch::RequestEntry) {
        // A log failure must never break a fetch.
        if let Err(e) = self.cache.log_request(&entry, self.max) {
            tracing::debug!("request log write failed: {e}");
        }
    }
}

fn parse_ts(s: &str) -> anyhow::Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(s)?.with_timezone(&Utc))
}

fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn blob_to_vec(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

/// Build a safe FTS5 MATCH expression: alphanumeric tokens, each quoted (so
/// punctuation/operators can't break the syntax), OR-joined for recall breadth.
fn fts_query(query: &str) -> String {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(" OR ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(url: &str, body: &str) -> Page {
        Page {
            url: url.to_string(),
            title: Some("T".into()),
            byline: None,
            markdown: body.into(),
            links: vec![Link { text: "x".into(), url: "https://e.test/x".into() }],
            forms: vec![],
            salvaged: false,
            js_required: false,
            content_hash: "abc".into(),
            markdown_alternate: None,
            source_format: None,
        }
    }

    #[test]
    fn put_then_get_roundtrips() {
        let c = Cache::open_in_memory().unwrap();
        assert!(c.get_page("https://e.test/").unwrap().is_none(), "miss before insert");
        c.put_page(&page("https://e.test/", "# hi"), Some("W/\"v1\""), None, false).unwrap();
        let row = c.get_page("https://e.test/").unwrap().unwrap();
        assert_eq!(row.page.markdown, "# hi");
        assert_eq!(row.etag.as_deref(), Some("W/\"v1\""));
        assert_eq!(row.page.links[0].url, "https://e.test/x");
        assert!(!row.pinned);
    }

    #[test]
    fn refresh_preserves_pin_and_updates_content() {
        let c = Cache::open_in_memory().unwrap();
        c.put_page(&page("https://e.test/", "old"), None, None, true).unwrap();
        c.put_page(&page("https://e.test/", "new"), None, None, true).unwrap();
        let row = c.get_page("https://e.test/").unwrap().unwrap();
        assert_eq!(row.page.markdown, "new");
        assert!(row.pinned, "pin survives a refresh");
        assert_eq!(c.page_count(), 1, "upsert, not a duplicate row");
    }

    #[test]
    fn delete_and_pin_toggle() {
        let c = Cache::open_in_memory().unwrap();
        c.put_page(&page("https://e.test/", "x"), None, None, false).unwrap();
        assert!(c.set_pinned("https://e.test/", true).unwrap());
        assert!(c.get_page("https://e.test/").unwrap().unwrap().pinned);
        assert!(c.delete_page("https://e.test/").unwrap());
        assert!(c.get_page("https://e.test/").unwrap().is_none());
        assert!(!c.delete_page("https://e.test/").unwrap(), "second delete is a no-op");
    }

    #[test]
    fn keyword_search_finds_pages_by_token() {
        let c = Cache::open_in_memory().unwrap();
        c.put_page(&page("https://e.test/rust", "the rust programming language is fast"), None, None, false).unwrap();
        c.put_page(&page("https://e.test/bread", "sourdough bread baking guide"), None, None, false).unwrap();
        let hits = c.keyword_search("rust language", 5).unwrap();
        assert_eq!(hits, vec!["https://e.test/rust".to_string()]);
        assert!(c.keyword_search("nonexistentword", 5).unwrap().is_empty());
        assert!(c.keyword_search("", 5).unwrap().is_empty(), "empty query → no match, no syntax error");
    }

    #[test]
    fn embedding_roundtrips_and_delete_clears_it() {
        let c = Cache::open_in_memory().unwrap();
        c.put_page(&page("https://e.test/x", "body"), None, None, false).unwrap();
        c.put_embedding("https://e.test/x", &[0.1, -0.2, 0.3]).unwrap();
        let all = c.all_embeddings().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].0, "https://e.test/x");
        assert!((all[0].1[1] - (-0.2)).abs() < 1e-6, "f32 blob roundtrips");
        c.delete_page("https://e.test/x").unwrap();
        assert!(c.all_embeddings().unwrap().is_empty(), "delete clears the embedding");
        assert!(c.keyword_search("body", 5).unwrap().is_empty(), "delete clears the FTS row");
    }

    // ---- distillations -----------------------------------------------------

    fn distillation(summary: &str, tags: &[&str]) -> Distillation {
        Distillation {
            summary:    summary.into(),
            key_points: vec!["point one".into()],
            entities:   vec!["ACME Corp".into()],
            tags:       tags.iter().map(|s| s.to_string()).collect(),
            model:      "test-model".into(),
            backend:    "ollama",
        }
    }

    #[test]
    fn distillation_roundtrips_and_delete_cascades() {
        let c = Cache::open_in_memory().unwrap();
        c.put_page(&page("https://e.test/x", "body"), None, None, false).unwrap();
        c.put_distillation("https://e.test/x", &distillation("A summary.", &["rust"]), "abc").unwrap();
        let row = c.get_distillation("https://e.test/x").unwrap().unwrap();
        assert_eq!(row.summary, "A summary.");
        assert_eq!(row.tags, vec!["rust"]);
        assert_eq!(row.content_hash, "abc");
        assert_eq!(row.model.as_deref(), Some("test-model"));
        assert_eq!(c.stats().distilled, 1);
        c.delete_page("https://e.test/x").unwrap();
        assert!(c.get_distillation("https://e.test/x").unwrap().is_none(), "cascade");
        assert_eq!(c.stats().distilled, 0);
    }

    #[test]
    fn undistilled_tracks_missing_and_stale_hash() {
        let c = Cache::open_in_memory().unwrap();
        c.put_page(&page("https://e.test/a", "body a"), None, None, false).unwrap();
        c.put_page(&page("https://e.test/b", "body b"), None, None, false).unwrap();
        assert_eq!(c.undistilled_count().unwrap(), 2, "nothing distilled yet");

        // page() uses content_hash "abc" — distill b against the matching hash.
        c.put_distillation("https://e.test/b", &distillation("S.", &[]), "abc").unwrap();
        assert_eq!(c.undistilled_urls(10).unwrap(), vec!["https://e.test/a"]);

        // The page content changes (new hash) → b needs re-distillation.
        let mut changed = page("https://e.test/b", "new body");
        changed.content_hash = "def".into();
        c.put_page(&changed, None, None, false).unwrap();
        assert_eq!(c.undistilled_count().unwrap(), 2, "hash mismatch marks it stale");
    }

    #[test]
    fn keyword_search_finds_pages_via_distilled_terms() {
        let c = Cache::open_in_memory().unwrap();
        c.put_page(&page("https://e.test/p", "an article about memory systems"), None, None, false).unwrap();
        // "cerebro" appears nowhere in the body — only in the distillation tags.
        assert!(c.keyword_search("cerebro", 5).unwrap().is_empty());
        c.put_distillation("https://e.test/p", &distillation("About Cerebro.", &["cerebro"]), "abc").unwrap();
        assert_eq!(c.keyword_search("cerebro", 5).unwrap(), vec!["https://e.test/p"]);
        // A body hit is not duplicated by its distillation hit.
        assert_eq!(c.keyword_search("memory systems cerebro", 5).unwrap().len(), 1);
    }

    // ---- forms + snapshots (Phase 12) --------------------------------------

    #[test]
    fn forms_roundtrip_through_the_page_store() {
        use crate::extract::{Form, FormField};
        let c = Cache::open_in_memory().unwrap();
        let mut p = page("https://e.test/f", "body");
        p.forms = vec![Form {
            idx:    1,
            action: "https://e.test/search".into(),
            method: "get".into(),
            fields: vec![FormField { name: "q".into(), kind: "text".into(), ..Default::default() }],
            submit: Some("Go".into()),
            submittable: true,
        }];
        c.put_page(&p, None, None, false).unwrap();
        let row = c.get_page("https://e.test/f").unwrap().unwrap();
        assert_eq!(row.page.forms, p.forms, "forms JSON roundtrips");
    }

    #[test]
    fn stale_optimistic_submittable_is_recomputed_on_read() {
        use crate::extract::{Form, FormField};
        let c = Cache::open_in_memory().unwrap();
        let mut p = page("https://e.test/stale", "body");
        // Simulate a pre-flag cache row: the stored flag says true (serde
        // default on old rows), but the fields say the form is dead. The
        // read path must serve the truth, not the stored optimism (apex1
        // seam report, 2026-07-26).
        p.forms = vec![Form {
            idx:    1,
            action: "https://e.test/".into(),
            method: "get".into(),
            fields: vec![FormField { name: String::new(), kind: "search".into(), ..Default::default() }],
            submit: None,
            submittable: true, // the stale lie
        }];
        p.markdown_alternate = Some("https://e.test/stale.md".into());
        c.put_page(&p, None, None, false).unwrap();
        let row = c.get_page("https://e.test/stale").unwrap().unwrap();
        assert!(
            !row.page.forms[0].submittable,
            "submittable is recomputed from the stored fields on every read"
        );
        assert_eq!(
            row.page.markdown_alternate.as_deref(),
            Some("https://e.test/stale.md"),
            "the markdown alternate survives the cache"
        );
    }

    #[test]
    fn salvage_flags_roundtrip_through_the_page_store() {
        let c = Cache::open_in_memory().unwrap();
        let mut p = page("https://e.test/spa", "salvaged body");
        p.salvaged = true;
        p.js_required = false;
        c.put_page(&p, None, None, false).unwrap();
        let row = c.get_page("https://e.test/spa").unwrap().unwrap();
        assert!(row.page.salvaged && !row.page.js_required, "flags survive the cache");
    }

    #[test]
    fn snapshot_roundtrips_ttl_prunes_and_delete_cascades() {
        let c = Cache::open_in_memory().unwrap();
        c.put_page(&page("https://e.test/s", "body"), None, None, false).unwrap();
        c.put_snapshot("https://e.test/s", "<html>raw</html>").unwrap();
        let (html, _ts) = c.get_snapshot("https://e.test/s").unwrap().unwrap();
        assert_eq!(html, "<html>raw</html>");
        assert_eq!(c.stats().snapshots, 1);

        assert_eq!(c.gc_snapshots(3600).unwrap(), 0, "fresh snapshot survives its TTL");
        assert_eq!(c.gc_snapshots(0).unwrap(), 1, "expired snapshot is pruned");
        assert!(c.get_snapshot("https://e.test/s").unwrap().is_none());

        c.put_snapshot("https://e.test/s", "<html>again</html>").unwrap();
        c.delete_page("https://e.test/s").unwrap();
        assert!(c.get_snapshot("https://e.test/s").unwrap().is_none(), "delete cascades");
        assert_eq!(c.stats().snapshots, 0);
    }

    #[test]
    fn request_log_records_newest_first_and_stays_bounded() {
        use crate::fetch::RequestEntry;
        let c = Cache::open_in_memory().unwrap();
        let entry = |url: &str, status: Option<u16>, err: Option<&str>| RequestEntry {
            at:          Utc::now(),
            method:      "GET".into(),
            url:         url.into(),
            status,
            wait_ms:     120,
            duration_ms: 45,
            error:       err.map(str::to_string),
        };
        for i in 0..10 {
            c.log_request(&entry(&format!("https://e.test/{i}"), Some(200), None), 5).unwrap();
        }
        let rows = c.recent_requests(10).unwrap();
        assert_eq!(rows.len(), 5, "bounded to the newest 5");
        assert_eq!(rows[0].url, "https://e.test/9", "newest first");
        assert_eq!(rows[0].wait_ms, 120, "politeness wait recorded");
        assert_eq!(c.stats().requests, 5);

        // A refusal is part of the trail.
        c.log_request(&entry("https://e.test/blocked", None, Some("blocked by robots.txt")), 5).unwrap();
        let rows = c.recent_requests(1).unwrap();
        assert!(rows[0].status.is_none());
        assert_eq!(rows[0].error.as_deref(), Some("blocked by robots.txt"));

        // max = 0 disables the log entirely.
        let c2 = Cache::open_in_memory().unwrap();
        c2.log_request(&entry("https://e.test/x", Some(200), None), 0).unwrap();
        assert_eq!(c2.stats().requests, 0);
    }

    #[test]
    fn old_schema_db_gains_the_forms_column_on_open() {
        // A DB created before Phase 12: pages without `forms`.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE pages (
                    url TEXT PRIMARY KEY, title TEXT, byline TEXT,
                    markdown TEXT NOT NULL, links TEXT NOT NULL,
                    content_hash TEXT NOT NULL, etag TEXT, last_modified TEXT,
                    fetched_at TEXT NOT NULL, last_access TEXT NOT NULL,
                    access_count INTEGER NOT NULL DEFAULT 0,
                    salience REAL NOT NULL DEFAULT 1.0,
                    pinned INTEGER NOT NULL DEFAULT 0);
                 INSERT INTO pages (url, markdown, links, content_hash, fetched_at, last_access)
                 VALUES ('https://e.test/old', 'legacy body', '[]', 'h',
                         '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');",
            )
            .unwrap();
        }
        let c = Cache::open(&path).unwrap();
        let row = c.get_page("https://e.test/old").unwrap().unwrap();
        assert_eq!(row.page.markdown, "legacy body", "legacy row readable after migration");
        assert!(row.page.forms.is_empty(), "migrated column defaults to no forms");
        // And the migrated table accepts new-format writes.
        c.put_page(&page("https://e.test/new", "x"), None, None, false).unwrap();
    }

    #[test]
    fn search_cache_roundtrips() {
        let c = Cache::open_in_memory().unwrap();
        let results = vec![SearchResult {
            title: "R".into(), url: "https://e.test/r".into(), snippet: "s".into(), rank: 0,
        }];
        c.put_search("ddg|5|rust", "rust", &results).unwrap();
        let (got, _ts) = c.get_search("ddg|5|rust").unwrap().unwrap();
        assert_eq!(got, results);
        assert!(c.get_search("ddg|5|other").unwrap().is_none());
    }
}
