# Occipital-RS — Agent & Developer Guide

> The agent's reading cortex. A pure-Rust web layer that gives an AI **polite eyes** on the
> live web and a **decaying memory** of what it read — search, fetch, read-mode extraction,
> and a Cerebro-style knowledge cache, behind MCP / CLI / REST.
>
> Cerebro is episodic memory; **Occipital is the visual/reading cortex** — what the agent sees
> and reads of the outside world. Sibling project, same shape, same biology metaphor.

See also: [docs/architecture.md](docs/architecture.md) · [docs/build-roadmap.md](docs/build-roadmap.md) · [docs/politeness.md](docs/politeness.md) · [docs/follow-along.md](docs/follow-along.md)

Sibling repos: `../CerebroCortex-RS` (the memory cortex — mirror its crate shape & conventions) · `../ApexOS-RS` (the primary consumer — Slint UI + agentd).

---

## What this is

A **single capability, three faces**, mirroring Cerebro:

1. **A polite fetcher + search engine** — curl-style HTTP, but a *good web citizen*: per-domain
   rate limiting, jitter, backoff, robots-awareness, honest UA. Reader-mode extraction turns
   HTML into clean markdown so the *same* pipeline feeds the agent (a readable search tool) and
   the human (a skim view). This is the anti-"AI fires off suspicious request volumes" guard.
2. **A read-through knowledge cache** — fetched pages are stored, embedded, and **decay over
   time** (web content rots). Every search hits the cache first; only a miss/stale entry goes
   live. Queryable like Cerebro (semantic + keyword), with salience/forgetting so stale pages
   don't flood results.
3. **A follow-along surface** (lives in ApexOS-RS, not here) — the desktop renders the agent's
   reader-mode view so a human skims along; a link click steers the agent ("go here").

**Standalone is a first-class goal.** Like Cerebro, any MCP/CLI/REST consumer can adopt
Occipital with zero ApexOS dependency. ApexOS-RS is just the first (and richest) consumer.

```
┌──────────────────────── Occipital-RS workspace ─────────────────────────┐
│                                                                          │
│   occipital (lib)  ── politeness · reader-mode · providers · cache ·     │
│                       embeddings · decay/ranking                         │
│        │                                                                 │
│        ├── occipital-mcp   stdio JSON-RPC  → any MCP client (APEX)        │
│        ├── occipital-api    axum REST       → management / dashboards     │
│        └── occipital-cli    clap            → ops, key CRUD, queries      │
└──────────────────────────────────────────────────────────────────────────┘
                 ▲ consumed by ApexOS-RS via register_mcp_server
                 ▼ follow-along UI + "go here" steer live in ApexOS-RS
```

Workspace layout (mirrors `CerebroCortex-RS`):

```
crates/
  occipital/        # core lib: fetch · politeness · extract · providers · cache · decay
  occipital-mcp/    # hand-rolled newline-delimited JSON-RPC over stdio (no SDK)
  occipital-api/    # axum REST (management + query)
  occipital-cli/    # clap CLI (ops, provider keys, queries)
docs/               # design docs (this is the planning lock-in)
```

---

## Locked decisions

- **Language**: Rust — every crate. Pure-Rust, no Chromium/headless-browser dependency.
- **Repo model**: standalone, Cerebro-sibling. ApexOS-RS consumes via MCP, never vendors it.
- **HTTP**: `reqwest` (async, rustls), one shared client with the politeness layer in front.
- **Reader-mode**: HTML → Markdown extraction (readability-style boilerplate strip + link list).
  The agent and the human see the *same* clean view — never raw HTML, never a real webview.
- **No real browser engine**: ApexOS's follow-along window is Slint (no webview — see ApexOS
  gotchas). "Render in a browser window" = render the reader-mode markdown + a live link list.
- **Search**: pluggable `SearchProvider` trait. **Default = polite scrape** (DuckDuckGo-HTML /
  SearXNG, no key, Nano-friendly). **Keyed providers opt-in** (Brave / Tavily / Bing).
- **Storage**: SQLite + FTS5, optional vector embeddings — same stack & idioms as Cerebro.
- **Embedding model**: `BAAI/bge-small-en-v1.5` (Micro+). `OCCIPITAL_EMBED_MODEL=""` → FTS5-only
  (Nano, ~no extra RSS). Mirror Cerebro's Nano/Micro tiers exactly.
- **Decay**: cached pages lose salience with age + disuse; a periodic GC prunes stale, unread
  pages (Cerebro's dream-pruning analog). Web content is perishable by default.
- **Politeness is non-negotiable**: identify honestly, pace conservatively. Anti-detection is
  achieved by *good citizenship* (rate, robots, backoff), **not** by deception/cloaking.

---

## Platform tiers (mirror ApexOS / Cerebro)

| Tier | Embeddings | Cache | Search |
|------|-----------|-------|--------|
| Nano | off (default build) | FTS5 keyword only | scrape providers |
| Micro+ | bge-small, build `--features embeddings` + set `OCCIPITAL_EMBED_MODEL` | cosine semantic + FTS5 | scrape + keyed (if keys) |

**Embeddings are an opt-in build feature**, not just a runtime toggle: the default
build excludes the ONNX runtime (`fastembed`) entirely — smaller binary, no build-time
download, Nano-friendly. Micro+ nodes build `cargo build --features embeddings` (or
`-p occipital-mcp --features embeddings`) **and** set `OCCIPITAL_EMBED_MODEL`. Without
the feature, a set model is ignored (warned) and recall is FTS5 keyword.

**Design rule (inherited from ApexOS):** build for Nano first — graceful when embeddings are
off, no timeouts shorter than 30s, never assume a key is present.

---

## The data flow (read-through cache)

```
agent → web_search("...")            ┌─────────── occipital ───────────┐
  │                                  │  1. cache.lookup(query)          │
  │                                  │       fresh semantic/keyword hit?│
  │            ┌── HIT (fresh) ──────┤       → serve cached + embed rank│
  ▼            │                     │  2. MISS / STALE:                │
results ◄──────┤                     │       provider.search() (polite) │
               │                     │       fetch top-N (rate-limited) │
               └── refreshed ────────┤       extract → store → embed    │
                                     │       conditional-GET on stale   │
                                     └──────────────────────────────────┘
```

`web_fetch(url)` is the same minus the provider step: cache-first, conditional GET (ETag /
If-Modified-Since) to refresh cheaply, polite live fetch only on a real miss.

---

## Tool surface (MCP) — first cut

| Tool | Purpose |
|------|---------|
| `web_search` | query → ranked results (cache-first, then live providers) |
| `web_fetch` | url → reader-mode markdown + links + forms (cache-first, conditional refresh); state-blob SPAs salvage (`salvaged: true`), truly client-only pages flag `js_required: true` |
| `web_dom` | url → the element registry: links + forms with stable ordinals (the interaction handles); reports whether a raw-HTML snapshot is held |
| `web_click` | click by registry ordinal — `link:N` follows the link (polite GET), `form:N` submits that form with current values |
| `web_submit` | fill + submit a form by ordinal; GET rides the read-through cache, POST is deliberate-only, never auto-retried, never cached |
| `web_recall` | semantic/keyword query over **already-read** pages only (no live hit); a distilled page recalls as its summary + tags, not a raw snippet |
| `web_save` | force-cache a url (pin; exempt from decay until TTL) |
| `web_forget` | evict a url / matching set from the cache |
| `web_distill` | LLM-curate cached pages into knowledge (summary · key points · entities · tags); `url` → that page (fetch-if-uncached, free re-ask on unchanged content), no `url` → bounded sweep of pending pages. Explicit-only — nothing spends tokens on its own |

`web_search`/`web_fetch` emit a **follow-along event** (see docs/follow-along.md) so a consumer
UI can render what the agent is reading. Pure-MCP consumers ignore it.

The **agent-browsing expansion (Phases 12–16) is complete** — page model, interaction verbs,
SPA salvage, sessions & identity, and multi-step politeness hygiene; see
docs/agent-browsing.md. No JS engine — ever in-core; a render *sidecar* (`occipital-render`,
CDP-as-client) remains a documented door, not a scheduled phase.

---

## Environment variables (planned)

| Var | Default | Purpose |
|-----|---------|---------|
| `OCCIPITAL_DB` | `~/.local/share/occipital/occipital.db` | SQLite path |
| `OCCIPITAL_EMBED_MODEL` | `BAAI/bge-small-en-v1.5` | `""` → FTS5-only (Nano) |
| `OCCIPITAL_USER_AGENT` | `Occipital/<ver> (+repo; ApexOS web reader)` | honest, identifiable UA |
| `OCCIPITAL_RESPECT_ROBOTS` | `1` | honor robots.txt (per-origin, cached; CLI `--obey-robots`) |
| `OCCIPITAL_ROBOTS_TTL_SECS` | `3600` | how long a cached robots.txt stays authoritative |
| `OCCIPITAL_LOG_MAX` | `500` | request-log rows retained (`0` disables); `occipital log` / `GET /log` |
| `OCCIPITAL_RATE_PER_DOMAIN` | `0.5` | requests/sec per domain (token bucket + jitter) |
| `OCCIPITAL_MAX_CONCURRENCY` | `4` | global in-flight fetch cap |
| `OCCIPITAL_FRESH_TTL_SECS` | `86400` | default cache freshness window |
| `OCCIPITAL_SNAPSHOT_TTL_SECS` | `3600` | raw-HTML interaction-snapshot retention (working memory for the browsing verbs; pruned by `gc`) |
| `OCCIPITAL_COOKIES` | `0` | opt-in session cookie jar — one jar, one identity (never farming/rotation) |
| `OCCIPITAL_COOKIES_FILE` | `<data_dir>/occipital/cookies.json` | persisted jar (0600); session cookies stay in memory |
| `OCCIPITAL_HEADERS_FILE` | unset | per-domain extra request headers as JSON (`*` / `.suffix` / exact host); UA never overridable |
| `OCCIPITAL_PROXY` | unset | explicit proxy URL (topology, not evasion; system proxy is logged when in effect) |
| `OCCIPITAL_SEARCH_PROVIDER` | `duckduckgo` | `duckduckgo`/`searxng`/`brave`/`tavily`/`bing` |
| `OCCIPITAL_SEARXNG_URL` | unset | SearXNG instance base URL |
| `OCCIPITAL_<PROVIDER>_KEY` | unset | keyed-provider API key (per provider); overrides the key file |
| `OCCIPITAL_KEYS_FILE` | `<data_dir>/occipital/keys.json` | persisted provider-key store (0600); managed via `occipital keys set/list/rm` |
| `OCCIPITAL_CURATE_BACKEND` | `auto` | distillation LLM: `auto` (Ollama → Anthropic fallback) \| `ollama` \| `anthropic` \| `off` — mirrors Cerebro's `CEREBRO_VISION_BACKEND` tiering |
| `OCCIPITAL_CURATE_URL` | `http://localhost:11434` | Ollama endpoint — point at a LAN inference node to hot-swap the curation backend |
| `OCCIPITAL_CURATE_MODEL` | `llama3.2` | Ollama curation model (small text model) |
| `OCCIPITAL_CURATE_API_MODEL` | `claude-haiku-4-5` | Anthropic curation model (needs `ANTHROPIC_API_KEY`, inherited from the host process env — on an ApexOS node the plugin inherits agentd's) |
| `OCCIPITAL_AUTO_DISTILL` | `off` | background auto-curation in the resident servers: `off` \| `local` (Ollama-pinned — never spends API tokens) \| `on` (the configured backend, API fallback included). The "living" knob — pages distill themselves as they're read |
| `OCCIPITAL_AUTO_DISTILL_INTERVAL_SECS` | `300` | seconds between background sweep ticks (floor 30) |
| `OCCIPITAL_AUTO_DISTILL_CAP` | `50` | max distillations per rolling 24 h before auto pauses (counts explicit ones too — a total-spend guard; `0` = uncapped) |

Keys are managed via CLI/API CRUD too (stored 0600), not only env — mirror agentd's token file.

---

## Conventions (inherited)

- **Clippy-clean, always** (Cerebro's C-RS-013). Zero warnings, workspace-wide.
- **MCP**: hand-rolled newline-delimited JSON-RPC over stdio, protocol `2024-11-05`, no SDK —
  byte-for-byte the Cerebro-MCP pattern (reuse its dispatch/transport skeleton).
- **Tests per crate**, build incrementally. Pure logic (politeness math, ranking, extraction)
  unit-tested without network; network paths behind a `Fetcher` trait + a mock.
- **Git**: feature branch → PR, never commit to `main` (ApexOS house rule). End commits with the
  `Co-Authored-By` trailer.
- **Docs travel with code.** Update CLAUDE.md + the relevant `docs/*.md` in the same PR.

---

## Cerebro session protocol (mandatory)

Same as the sibling repos. Agent `FORGE` (agent_id=`"FORGE"`, ⚒) for all Cerebro calls.

**START** — `session_recall(query="Occipital-RS build status step progress", agent_id="FORGE")`.
**END** — `session_save(...)` + `store_procedure`/`store_intention` as needed.

---

## Boundary: Occipital vs Cerebro vs ApexOS

- **Cerebro** remembers the agent's *own* experience (episodes, skills, intentions). Private,
  earned, decays by FSRS/ACT-R.
- **Occipital** remembers the *outside world* the agent read (web pages). Public, fetched,
  decays by staleness. Different concern → **separate DB**, but the **same embedding model +
  storage idioms** so one node runs both with a single model loaded.
- **ApexOS-RS** wires both in (MCP) and adds the human-facing surfaces (the follow-along browser
  window + the "go here" steer). The web *tools* are Occipital's; the *window* is ApexOS's.

---

## Docs

| File | Load when working on |
|------|----------------------|
| `docs/architecture.md` | crate graph, the cache/decay model, storage schema, integration points |
| `docs/build-roadmap.md` | phased build order with gates (Phase 0–9) |
| `docs/politeness.md` | the scraping-etiquette contract — rate limits, robots, UA, backoff |
| `docs/agent-browsing.md` | the browsing expansion (Phases 12–16) — page model, click/submit verbs, SPA salvage, sessions, the JS-door contract |
| `docs/follow-along.md` | the agent↔UI contract — web_view events + the "go here" steer protocol |
