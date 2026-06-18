# Occipital-RS — Architecture

The core lib `occipital` is a pipeline with a cache wrapped around it. The three binaries
(`-mcp`, `-api`, `-cli`) are thin shells over the same lib, exactly like Cerebro.

```
                         ┌──────────────────────── occipital (lib) ───────────────────────┐
  web_search / web_fetch │                                                                 │
  ───────────────────────►   Cache  ──hit?──►  Ranker (relevance × freshness × salience)   │
                         │     │ miss/stale                                  │             │
                         │     ▼                                             │             │
                         │   Providers ──► Fetcher ──► Extractor ──► Store ──┘             │
                         │  (search)      (politeness)  (reader-mode) (sqlite+embeddings)  │
                         └─────────────────────────────────────────────────────────────────┘
```

## Modules (in `occipital`)

| Module | Responsibility |
|--------|----------------|
| `fetch` | the polite HTTP layer — one `reqwest` client behind a per-domain rate limiter, jitter, backoff, robots cache, honest UA. The only thing that touches the network. Behind a `Fetcher` trait so everything above it is testable with a mock. |
| `extract` | HTML → `Page { title, byline, markdown, links, content_hash }`. Readability-style boilerplate strip; never returns raw HTML. |
| `providers` | the `SearchProvider` trait + impls: `duckduckgo` (HTML scrape), `searxng` (JSON), `brave`/`tavily`/`bing` (keyed). Normalizes to `Vec<SearchResult>`. |
| `cache` | the read-through store: SQLite (`pages`, `searches`), FTS5, optional embeddings. First-hit lookup, conditional-GET refresh, write-back. |
| `decay` | salience update + GC. Web pages lose salience by age + disuse; the GC prunes stale, unread, low-salience pages. The Cerebro-dream-pruning analog. |
| `rank` | result ordering: `relevance × freshness × salience`. Keeps stale cached pages from outranking fresh signal in semantic/keyword search. |
| `config` | env + file config; provider keys (0600); tier detection (Nano/Micro+). |

## Storage schema (first cut)

SQLite, same idioms as Cerebro (`CerebroCortex-RS/crates/cerebro/src/storage`).

```sql
pages (
  url           TEXT PRIMARY KEY,     -- canonicalized
  title         TEXT,
  markdown      TEXT,                 -- reader-mode body
  links         TEXT,                 -- JSON array of {text,url}
  content_hash  TEXT,                 -- dedup + change detection
  etag          TEXT,                 -- conditional GET
  last_modified TEXT,                 -- conditional GET
  fetched_at    TEXT,                 -- RFC3339
  last_access   TEXT,
  access_count  INTEGER DEFAULT 0,
  salience      REAL    DEFAULT 1.0,  -- decays with age + disuse
  pinned        INTEGER DEFAULT 0     -- web_save → exempt from GC
)
pages_fts (url, title, markdown)      -- FTS5, keyword fallback (Nano)
embeddings (url, vec BLOB)            -- bge-small, Micro+ only
searches (                            -- optional: remember query→results for warm cache
  query_hash TEXT PRIMARY KEY, query TEXT, results TEXT, ts TEXT
)
```

## Freshness & the read-through contract

1. **`web_fetch(url)`** → canonicalize → `cache.get(url)`.
   - fresh (`now - fetched_at < TTL`) → serve cached, bump `last_access`/`access_count`.
   - stale → **conditional GET** (`If-None-Match`/`If-Modified-Since`). `304` → refresh
     `fetched_at` only (cheap, polite). `200` → re-extract + re-embed + store.
   - miss → polite live fetch → extract → store → embed → serve.
2. **`web_search(query)`** → `cache.search(query)` (semantic if embeddings, else FTS5).
   - enough fresh, high-rank hits → serve from cache (no network at all).
   - else → `provider.search()` → fetch top-N through step 1 → store the result set → rank.

The cache-first rule is *also* a politeness multiplier: repeated/again-asked queries cost zero
live requests.

## Decay model

`salience(t) = salience₀ · exp(-Δage/τ_age) · recency_boost(last_access)` — tunable τ per
content class later. A periodic GC (CLI `occipital gc`, or an interval task) soft-deletes pages
under a salience floor that are unpinned and unread for N days. This is deliberately the same
*shape* as Cerebro's FSRS decay + dream pruning, so the mental model transfers — but tuned for
**perishable** web content (shorter half-life than lived experience).

## Embeddings

Optional, gated by `OCCIPITAL_EMBED_MODEL`. Same `bge-small` model and loader conventions as
Cerebro so a co-located node loads **one** model. Nano (`""`) → FTS5 keyword only, no extra RSS.
Independent DB from Cerebro (different concern), shared model.

## Integration with ApexOS-RS

- agentd registers `occipital-mcp` via the existing `register_mcp_server` mechanism → APEX gets
  `web_search`/`web_fetch`/`web_recall`/… with **zero agentd code changes**.
- The follow-along window + "go here" steer are an ApexOS-RS concern — see
  [follow-along.md](follow-along.md). Occipital only *emits* a structured view event in its tool
  result; ApexOS decides how to render and how to route a steer back.

## Crate dependency graph

```
occipital-mcp ─┐
occipital-api ─┼─► occipital (lib) ─► reqwest, rusqlite, scraper/readability, (bge embedder)
occipital-cli ─┘
```

No crate depends on ApexOS or Cerebro. (A future optional `occipital-embed` feature may reuse
Cerebro's embedder crate if it's published separately; until then Occipital carries its own.)
