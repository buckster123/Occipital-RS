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

use crate::extract::{Link, Page};
use crate::providers::SearchResult;

/// Cache size counters (for `stats`).
#[derive(Debug, Clone, Serialize)]
pub struct CacheStats {
    pub pages:      i64,
    pub pinned:     i64,
    pub embeddings: i64,
    pub searches:   i64,
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
        Ok(Self { conn: Mutex::new(conn) })
    }

    // ---- pages -----------------------------------------------------------

    pub fn get_page(&self, url: &str) -> anyhow::Result<Option<PageRow>> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT title, byline, markdown, links, content_hash, etag, last_modified, \
                        fetched_at, pinned FROM pages WHERE url = ?1",
                params![url],
                |r| {
                    let links_json: String = r.get(3)?;
                    let fetched_at: String = r.get(7)?;
                    Ok((
                        r.get::<_, Option<String>>(0)?, // title
                        r.get::<_, Option<String>>(1)?, // byline
                        r.get::<_, String>(2)?,         // markdown
                        links_json,
                        r.get::<_, String>(4)?,         // content_hash
                        r.get::<_, Option<String>>(5)?, // etag
                        r.get::<_, Option<String>>(6)?, // last_modified
                        fetched_at,
                        r.get::<_, i64>(8)? != 0,        // pinned
                    ))
                },
            )
            .optional()?;

        let Some((title, byline, markdown, links_json, content_hash, etag, last_modified, fetched_at, pinned)) = row
        else {
            return Ok(None);
        };
        let links: Vec<Link> = serde_json::from_str(&links_json).unwrap_or_default();
        let fetched_at = parse_ts(&fetched_at)?;
        Ok(Some(PageRow {
            page: Page { url: url.to_string(), title, byline, markdown, links, content_hash },
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
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO pages \
               (url, title, byline, markdown, links, content_hash, etag, last_modified, \
                fetched_at, last_access, access_count, salience, pinned) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?9,0,1.0,?10) \
             ON CONFLICT(url) DO UPDATE SET \
               title=excluded.title, byline=excluded.byline, markdown=excluded.markdown, \
               links=excluded.links, content_hash=excluded.content_hash, etag=excluded.etag, \
               last_modified=excluded.last_modified, fetched_at=excluded.fetched_at, \
               last_access=excluded.last_access, pinned=excluded.pinned",
            params![
                page.url, page.title, page.byline, page.markdown, links, page.content_hash,
                etag, last_modified, now, pinned as i64
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

    /// FTS5 keyword search over cached pages → matching urls, best-ranked first.
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
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
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
        Ok(removed)
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
            content_hash: "abc".into(),
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
