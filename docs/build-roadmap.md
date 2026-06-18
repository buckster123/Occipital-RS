# Occipital-RS — Build Roadmap

Phased, each phase has a **gate** (the thing that must work to move on). Phases 0–8 are the
standalone Occipital-RS; Phase 9 is the ApexOS-RS integration. Mirror Cerebro's discipline:
clippy-clean, tests per crate, build incrementally.

| Phase | Feature | Gate | Status |
|-------|---------|------|--------|
| 0 | **Scaffold** — Cargo workspace (`occipital` + `-mcp`/`-api`/`-cli`), config module, tier detection, CI clippy-clean | `cargo build --workspace` green; `occipital-mcp` does the MCP handshake (initialize/tools-list) | ✓ — workspace builds; `occipital-mcp` handshake + 5-tool `tools/list` live (honest not-implemented `tools/call`); `Config::from_env` + Nano/Micro tier + compile-time polite-defaults guard; CLI `status`; `-api` `/health`; 9 tests, clippy `-D warnings` clean; CI workflow added |
| 1 | **Polite fetcher** — `Fetcher` trait, `reqwest` client, per-domain token bucket + jitter + concurrency cap + backoff, robots cache, honest UA | fetch a URL; an integration test proves N requests to one domain are spaced ≥ the interval; robots `Disallow` honored | ✓ — `Fetcher` trait + `PoliteFetcher` (rustls reqwest, honest UA, redirect cap, body cap); per-domain `DomainLimiter` with **additive-only** jitter (floor never violated), global concurrency `Semaphore`, polite backoff honoring `Retry-After`; cached robots parser (UA groups, longest-match Allow-override, `*`/`$`). 19 unit tests (spacing + robots gate pure-tested) + a live `example.com` fetch; clippy `-D warnings` clean |
| 2 | **Reader-mode** — HTML → `Page{title,markdown,links,content_hash}` | a real page returns clean markdown + a sane link list, no chrome; emoji/CJK safe | ✓ — single-pass DOM→Markdown via `scraper`: `select_main` (largest `<main>`/`<article>`/`[role=main]`, body fallback), boilerplate skip-list, common-element conversion (headings/bold/italic/lists/code/pre/blockquote/img), links resolved to absolute + deduped in the same pass, FNV-1a `content_hash`, depth-guarded. UTF-8 throughout (emoji/CJK safe). 9 fixture tests + a live `example.com` round-trip; clippy `-D warnings` clean |
| 3 | **Search providers** — `SearchProvider` trait + `duckduckgo` (HTML) + `searxng` (JSON); `web_search`/`web_fetch` MCP tools | `web_search("…")` returns ranked results live; `web_fetch` returns reader-mode | ☐ |
| 4 | **Cache (read-through)** — SQLite `pages`, FTS5, first-hit lookup, conditional GET, write-back | second fetch served from cache; stale entry refreshes via `304`; re-asked search makes zero live requests | ☐ |
| 5 | **Embeddings + semantic recall** — bge-small (gated), `web_recall` over read pages only | semantic query over cached pages (Micro+); FTS5 fallback works with `OCCIPITAL_EMBED_MODEL=""` (Nano) | ☐ |
| 6 | **Decay + ranking + GC** — salience decay, `relevance×freshness×salience` ranking, prune stale unread pages | an old unread page sinks in ranking and is GC'd; a `web_save`-pinned page survives | ☐ |
| 7 | **Keyed providers** — `brave`/`tavily`/`bing` + key CRUD (0600), provider selection | keyed search works when a key is set; falls back to scrape gracefully when not | ☐ |
| 8 | **API + CLI surfaces** — axum REST + clap CLI parity (query, page CRUD, provider keys, gc, stats) | full management + query surface; CLI `occipital search/fetch/recall/gc/keys` | ☐ |
| 9 | **ApexOS-RS integration** — `register_mcp_server` wire-up; `view`-payload follow-along window in `ui-slint`; "go here" click → `user_prompt` nudge | APEX searches the live web politely; a human follows along on the desktop and steers with a click | ☐ |

**Gate to move on:** the row's gate works end-to-end, clippy-clean, tested.

## Sequencing notes

- **Phases 0–3** are the minimum useful standalone: APEX (or any MCP client) can search + read
  the web politely. Ship this first — it's the headline capability.
- **Phases 4–6** turn it into "Cerebro for the web" — the cache, recall, and forgetting. This is
  what makes repeated/long-running research cheap and keeps results from rotting.
- **Phase 7** is opt-in quality for keyed nodes; deliberately after the scrape default works.
- **Phase 8** rounds out the standalone management story (parity with Cerebro's CLI/API).
- **Phase 9** is the only ApexOS-RS work and is purely additive (no agentd changes — see
  [follow-along.md](follow-along.md)).

## Build-time risks to watch (resolve as encountered)

- **Reader-mode crate choice.** Survey pure-Rust readability options (`readability`,
  `dom_smoothie`, `article_scraper`, or `scraper` + a hand-rolled boilerplate heuristic). Pick on
  output quality + maintenance + Nano weight. Keep it behind the `extract` module so it's swappable.
- **DuckDuckGo / SearXNG HTML drift.** Scrape parsers break when markup changes. Keep each
  provider's parser small + unit-tested against a saved fixture; fail soft (empty results + log),
  never panic.
- **Embedder reuse.** If Cerebro's embedder is extractable as a crate, depend on it (one model,
  one loader). Until then Occipital carries its own copy of the bge-small loader — keep the
  interface identical so a later swap is mechanical.
- **DB-share temptation.** Resist co-mingling with Cerebro's DB. Same model, **separate store** —
  web cache and episodic memory are different lifecycles (perishable vs earned).
- **Politeness vs. usefulness tension.** Conservative defaults may feel slow on a fast node;
  every limit is env-tunable, but the *defaults* stay polite. Document, don't loosen silently.

## Out of scope (for now)

- JS-rendered pages / headless browser (breaks pure-Rust + Nano; use a keyed provider instead).
- Authenticated / paywalled content.
- Image/PDF extraction (a later "Occipital vision" slice could mirror Cerebro's `describe_image`).
- Crawling / link-following beyond the explicit `top_n` search fetch.
