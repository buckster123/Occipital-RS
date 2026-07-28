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
| `fetch` | the polite HTTP layer — one `reqwest` client behind a per-registrable-domain rate limiter (eTLD+1 buckets, robots `Crawl-delay` raising the interval), jitter, backoff, an origin-keyed TTL'd robots cache, honest UA. The only thing that touches the network. Behind a `Fetcher` trait so everything above it is testable with a mock; a `RequestSink` seam feeds the request log without `fetch` knowing about SQLite. |
| `extract` | HTML → `Page { title, byline, markdown, links, forms, salvaged, js_required, content_hash }`. Readability-style boilerplate strip; never returns raw HTML as prose. Phase 12: forms are extracted document-wide with stable 1-based ordinals (the element registry) and render as one-line annotated blocks in the reader view — the interaction handles for the click/submit verbs. |
| `salvage` | the no-JS SPA path (Phase 14): when extraction is thin, mine `ld+json`, framework state blobs (`__NEXT_DATA__` etc. — strict JSON only, nothing executed), `noscript`, meta descriptions, and feeds; flag `salvaged` on success, `js_required` on a scripts-heavy shell with nothing recoverable. |
| `providers` | the `SearchProvider` trait + impls: `duckduckgo` (HTML scrape), `searxng` (JSON), `brave`/`tavily`/`bing` (keyed). Normalizes to `Vec<SearchResult>`. |
| `cache` | the read-through store: SQLite (`pages`, `searches`, `distillations`, `snapshots`), FTS5, optional embeddings. First-hit lookup, conditional-GET refresh, write-back. Snapshots (raw HTML, TTL-pruned) are interaction working memory, never recalled. |
| `curate` | the distillation layer (the knowledge hub): a tiered LLM `Distiller` (Ollama local/LAN → Anthropic API, mirroring Cerebro's `describe_image`) turns a cached page into summary/key-points/entities/tags. Explicit via `web_distill`/CLI/API; **opt-in background auto-curation** (`OCCIPITAL_AUTO_DISTILL` — `local` pins the sweep to Ollama so it never spends API tokens; rolling-24h budget cap) makes pages distill themselves as they're read. Recall serves the distillation over the raw body, and distilled terms are FTS-indexed so curation widens even Nano keyword recall. Talks to an inference endpoint, not the open web — plain reqwest, not the polite `Fetcher`. |
| `relate` | the connective layer: pure overlap scoring over distilled entities/tags (entities ×2, tags ×1, case-insensitive, zero-overlap = not related). Computed live from the `distillations` store — no link table to go stale. Surfaced as `web_related` / CLI `related` / `GET /related`, and inline (top-3 `related`) on every fresh distillation. Deliberately not cosine — shared entities are the knowledge-graph signal, embeddings say "the prose is alike" |
| `decay` | salience update + GC. Web pages lose salience by age + disuse; the GC prunes stale, unread, low-salience pages. The Cerebro-dream-pruning analog. |
| `rank` | result ordering: `relevance × freshness × salience`. Keeps stale cached pages from outranking fresh signal in semantic/keyword search. |
| `session` | sessions & identity (Phase 15): the opt-in persistent `CookieJar` (reqwest `CookieStore` impl with RFC-6265 matching; expiring cookies written 0600, session cookies memory-only) and `HeaderRules` (per-domain extra headers; UA/cookie/host never overridable). One jar, one identity. |
| `config` | env + file config; provider keys (0600); tier detection (Nano/Micro+). |

## Storage schema (first cut)

SQLite, same idioms as Cerebro (`CerebroCortex-RS/crates/cerebro/src/storage`).

```sql
pages (
  url           TEXT PRIMARY KEY,     -- canonicalized
  title         TEXT,
  markdown      TEXT,                 -- reader-mode body
  links         TEXT,                 -- JSON array of {text,url}
  forms         TEXT,                 -- JSON array of Form (the element registry, Phase 12)
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
distillations (                       -- LLM curation (the knowledge hub layer)
  url TEXT PRIMARY KEY,               -- cascade-deleted with the page
  summary TEXT, key_points TEXT,      -- key_points/entities/tags: JSON arrays
  entities TEXT, tags TEXT,
  content_hash TEXT,                  -- page hash distilled; mismatch = stale (re-distill)
  model TEXT, distilled_at TEXT
)
distill_fts (url, summary, terms)     -- FTS5 over curated text; unioned into keyword recall
requests (                            -- the honest trail (Phase 16), bounded
  id INTEGER PRIMARY KEY, at TEXT, method TEXT, url TEXT,
  status INTEGER,                     -- NULL when refused or errored
  wait_ms INTEGER, duration_ms INTEGER, error TEXT
)
snapshots (                           -- interaction working memory (Phase 12)
  url TEXT PRIMARY KEY,               -- cascade-deleted with the page
  html TEXT,                          -- raw fetched HTML (body-cap bounded)
  fetched_at TEXT                     -- RFC3339; pruned past OCCIPITAL_SNAPSHOT_TTL_SECS
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
