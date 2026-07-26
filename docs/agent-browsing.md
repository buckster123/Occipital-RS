# Occipital-RS — Agent Browsing (Phases 12–16)

> From polite **eyes** to polite **hands**. The browsing expansion gives the agent structured
> sight of a page's *interactive* surface (forms, buttons, inputs) and two careful verbs to act
> on it (click, submit) — without ever becoming a browser engine. Politeness stays
> non-negotiable: interactions ride the same rate limits, robots gate, and honest UA as reads.

Status: **design lock-in** (this doc is the plan; phases land as separate PRs).
Origin: André's agent-browsing jot (2026-07-25) mapped against the Phase-11 codebase.

---

## The fork — and which side we build

There are two roads to "an agent that can use the web", and they are wildly different sizes:

1. **Browser-engine emulation** — JavaScript execution, DOM APIs, XHR/Fetch emulation, a CDP
   server. This is Servo-scale work: a JS interpreter is the *easy* part; pages need the whole
   web platform (events, timers, `localStorage`, `IntersectionObserver`, `matchMedia`, …) and
   modern frameworks crash on the first missing API. It collides with two locked decisions
   (pure-Rust, no browser engine) and with the politeness stance (headless browsers invite
   fingerprint-evasion games we refuse to play).
2. **Structured interactive fetching** — keep the fetch-parse-extract pipeline, but stop
   throwing away the page's interactive structure. Extract forms and buttons, give elements
   stable handles, and implement *click* and *submit* as what they are in HTML terms: a polite
   GET of an `href`, a GET/POST of a form's `action`. Add cookies so multi-step flows hold
   together, and a salvage pass so state-blob SPAs still yield content.

**We build road 2.** Road 1 gets a *door* (a loader seam + an escalation contract, below), not
an implementation. The door's eventual occupant is a **sidecar driving a real headless
browser**, never an in-process JS engine — rationale in [The JS door](#the-js-door).

## Verdict on the original jot

| Jot item | Verdict | Disposition |
|---|---|---|
| HTTP loader | ✅ have it | `PoliteFetcher` (Phase 1) |
| HTML parser | ✅ have it | `scraper` (Phase 2) |
| DOM tree | 🔨 Phase 12 | retain a bounded snapshot + element registry (today the DOM is parsed then discarded) |
| DOM dump | 🔨 Phase 12 | serialize the element registry (`web_dom`) |
| Input form | 🔨 Phase 12+13 | extract forms (today `extract.rs` **skips** `form`/`input`/`button` subtrees), then `web_submit` |
| Click | 🔨 Phase 13 | `web_click` — link → polite GET; submit button → its form's submission |
| Cookies | 🔨 Phase 15 | reqwest `cookies` feature + persisted jar (0600, like `keys.json`) |
| Custom HTTP headers | 🔨 Phase 15 | per-domain header map (plumbing exists via `HttpRequest.headers`); UA stays honest |
| Proxy support | 🔨 Phase 15 | explicit `OCCIPITAL_PROXY`; topology, not evasion — one identity, no rotation |
| Network interception | 🔨 Phase 16 | we *are* the network stack → a queryable request log + follow-along trace |
| `--obey-robots` flag | ✅ mostly | `OCCIPITAL_RESPECT_ROBOTS` exists; Phase 16 adds the CLI alias + honors `Crawl-delay` for real |
| JavaScript support | ❌ skip | replaced by SPA salvage (Phase 14) + the render-sidecar door |
| DOM APIs | ❌ skip | only meaningful with JS running |
| Ajax (XHR / Fetch API) | ❌ skip | APIs for scripts we don't run |
| CDP/websockets **server** | ❌ skip | CDP is for driving a real browser; our surface is MCP. (The *door* uses CDP as a **client**.) |

## The page model (Phase 12)

Today `extract()` produces markdown + a flat link list and drops everything else. The browsing
expansion upgrades `Page` to carry the interactive surface:

- **Forms extracted, not skipped.** `Form { idx, action, method, fields }`, where fields keep
  name/type/label/current-value (hidden inputs preserved verbatim — they're how sites thread
  state). Rendered into the reader view as annotated blocks so agent *and* human see them:
  `[form#1 → GET /search — input "q" · submit "Go"]`.
- **Element registry.** Every interactive element (link, form, field, button) gets a stable
  ordinal within its snapshot. Verbs address elements by ordinal — no CSS selectors in the
  tool surface, no coordinates.
- **Bounded snapshots.** The raw HTML of a fetched page is retained (capped by the existing
  body limit) in a `snapshots` store with its own TTL + GC, so a later `web_click`/`web_submit`
  can resolve ordinals without a re-fetch. Snapshots are working memory, not knowledge — they
  decay fast and are never recalled.
- **`web_dom`** — dump the element registry (and optionally the sanitized DOM outline) for the
  rare case the agent needs more than reader-mode.

## Interaction verbs (Phase 13)

| Tool | Semantics |
|---|---|
| `web_click(url, element)` | Resolve ordinal in the page's snapshot. Link → polite GET of the resolved `href` (= `web_fetch`). Submit button → its form's submission with current values. |
| `web_submit(url, form, fields)` | Fill the named fields (unnamed keep defaults, hidden preserved), then GET or POST per the form's `method`/`action`, robots-gated and rate-limited like any fetch. Response goes through the same extract → cache → follow-along pipeline. |

Rules that keep this polite and safe:

- **POST is always deliberate.** Only an explicit `web_submit`/`web_click` fires one — never
  extraction, never cache refresh (conditional GET refresh applies to GET-obtained pages only).
- **POST is never auto-retried** (not idempotent). A 429/503 on POST returns honestly with the
  backoff hint instead of replaying.
- **Same budget.** Interactions consume the same per-domain token bucket and global
  concurrency as reads.
- **Follow-along shows hands, not just eyes.** New `kind` payloads (`"click"`, `"submit"`)
  so the ApexOS window renders "agent typed *rust politeness* into search and submitted".
- **POST results are addressable.** A POST result carries a `handle` (`result:<hash>`) usable
  as the `url` of the next verb — see *The field pass* below.

## SPA salvage + honest JS signaling (Phase 14)

The reason people want JS support is SPAs that ship an empty `<body>` and hydrate client-side.
But SEO forces most *content* sites to server-render, and most of the rest embed their state as
JSON in the HTML. When the extracted body text is suspiciously thin, a salvage pass mines:

- `__NEXT_DATA__` (Next.js), `window.__NUXT__`, `__PRELOADED_STATE__`/`__INITIAL_STATE__`
  (Redux), `__remixContext`, SvelteKit data scripts — framework state blobs
- `application/ld+json` (articles, products, recipes — structured by design for crawlers)
- OpenGraph/meta description, `<noscript>` fallbacks, RSS/Atom feed discovery

Salvaged content renders through the same markdown pipeline with a `salvaged: true` note. When
even salvage fails, the result carries **`js_required: true`** — the agent learns *why* the
page is thin instead of concluding it's blank, and a render-capable node (see the door) knows
to escalate. Zero script execution, zero new dependencies.

## Sessions & identity (Phase 15)

This phase consciously amends the roadmap's old "authenticated content: out of scope":

- **Cookie jar** — reqwest `cookies` feature; persisted per-node (0600 file beside
  `keys.json`), opt-in via `OCCIPITAL_COOKIES=1`. One jar, one identity — sessions make
  multi-step flows (consent walls, searches, operator-provisioned logins) work; they are
  **not** a farming/rotation mechanism. CLI: `occipital cookies list/clear [domain]`.
- **Operator-provisioned auth only.** The *operator* may import/establish a session on their
  own accounts (their infra, their call). The agent never solves CAPTCHAs, never bypasses
  paywalls, never harvests credentials.
- **Per-domain custom headers** — config map for the legit cases (API-ish endpoints, language
  hints). The UA line stays honest and is not overridable per-request (locked).
- **Proxy** — explicit `OCCIPITAL_PROXY` (system `HTTP(S)_PROXY` already honored by reqwest
  today, silently; make it explicit and logged). Topology, not evasion: one proxy, no rotation.

## Politeness hygiene for multi-step browsing (Phase 16)

Gaps that matter more once sessions span many requests — all already flagged in-code:

- **Honor `Crawl-delay`** — parsed since Phase 1, never consumed; politeness.md already
  promises it. It raises that domain's token-bucket interval.
- **Bucket by eTLD+1**, not raw host — subdomain hops shouldn't evade a site's budget.
- **Robots cache TTL** — today it's process-lifetime; long-running nodes should re-check.
- **Request log** — every fetch (verb, URL, status, timing, bucket wait) into a bounded table;
  `occipital log` + a follow-along trace payload. This is "network interception" done honestly:
  we own the network layer, so observability is a feature, not a hack.
- **`--obey-robots`** CLI alias for `OCCIPITAL_RESPECT_ROBOTS` (default stays *on*).

## The field pass (2026-07-26) — first live smoke, four findings

apex1 drove the verbs against live search sites hours after they deployed and filed four
findings (verified against this source; the write-up lives on that node). What shipped:

- **Result handles (the structural one).** A POST result was unaddressable — `click`/`submit`
  resolve ordinals via the *source* URL, and the POST result is deliberately never cached — so
  interaction depth capped at one hop (a POST-paginated SERP was walkable to page 2 and no
  further). Fix: the engine keeps a small in-memory **result store** (16 entries, ~15 min TTL,
  gone on restart — working memory, not knowledge); a POST result mints an opaque
  `result:<hash>` **handle**, returned in the report, accepted as the `url` of any verb. The
  durable cache's "POST is never cached" invariant is untouched, and the source URL's registry
  is never overwritten (the naive `final_url` keying would have collided with the landing page).
- **Link-list cap on the wire.** One `web_dom` on a portal page returned 570 links (400+
  interlanguage). The MCP layer now windows links (default first 120) with `links_total`
  always reporting the true count; `web_dom` pages via `links_from`/`limit`. Ordinals are
  positions in the FULL list — the window is a view, never a re-index.
- **Redirector unwrapping moved down to extraction.** Only the search-provider path unwrapped
  DDG's `/l/?uddg=` wrappers, so the same site yielded clean or wrapped hrefs depending on the
  verb that reached it. `extract()` now unwraps known redirectors (DDG `uddg`, Google `/url`
  `q`/`url`, `out.reddit.com` `url`) for every link, conservatively: known host + a parameter
  that is itself a full http(s) URL, nothing else touched (Bing's base64 `aclick` stays out).
- **Formless-search honesty.** A search-looking `<input>` outside any `<form>` (openlibrary.org's
  header box) is script-driven and cannot be submitted — form *collection* was already
  document-wide (the finding's hypothesized `select_main` scoping wasn't the cause), so the
  fix is a reader-view note: `[a search input exists outside any form — script-driven; not
  submittable via the interaction verbs]`.

### The verification round (same day)

apex1 re-ran the repros post-merge: a **three-hop POST walk** on DDG lite (hidden state
`s=10 → s=25 → s=40` round-tripping, the fatal ordinal shift now navigable), windowing
verified against the morning's uncapped 570-link dump, unwrapping clean on organics with
sponsored links staying honestly wrapped. Two follow-ups from that round:

- **The affordance widening.** openlibrary.org ships *zero* `<input>` — its search bar is a
  `<div class="search-bar-trigger">` hydrated by JS, so the input-based note could never fire
  on the page that motivated it. The trigger now also fires when a page yields **zero
  submittable forms** but its markup carries a search affordance (`role="search"`, or a
  class/id/aria/data hint like `search-bar`/`searchbox`/`search-trigger` — compound hints
  only, so "research" can't trip it). Wording: "*a search affordance exists outside any
  form*".
- **`from_handle` audit echo.** Every hop of a POST walk shares the same `final_url`, so
  `source_url` made three hops read as identical submits. A handle-sourced `web_click`/
  `web_submit` now echoes the input handle as `from_handle` — the chain self-documents.

Parked from that round: `js_required` stays false on hydration-failure pages with enough
residual links (the affordance note now carries the "don't bother with the verbs here"
signal); Bing `aclick` unwrapping (base64 blob, sponsored-only — verified a non-issue in the
field).

### Round three — the dead-form bug and the survey tells

apex1's third report verified `from_handle` and the affordance widening in the field, then
caught a NEW bug and surveyed six docs sites for what search markup actually looks like:

- **Dead forms refused, not no-op'd.** react.dev's "search" forms are Algolia modal
  triggers: one unnamed text field, action = the page itself. Submitting one collapsed to a
  bare re-fetch returned as the "result" — an **affirmative false answer** ("react.dev has
  no docs on X"). Structural fix: a GET form with zero NAMED fields cannot carry data by
  construction — `web_submit` now refuses it honestly, and the registry marks it
  `submittable: false` up front. Empty-field POST stays allowed (a deliberate act with a
  live status).
- **Survey-driven tells.** The compound-class hints matched 1/6 surveyed sites; 6/6 carried
  `aria-label="Search"` — an accessibility requirement, not a styling choice. Added:
  aria-label word-match ("Research papers" can't trip it), `docsearch` (Algolia DocSearch —
  Astro/Vite/Tailwind/Prisma/Vue), custom-element tags ending `-search` (`<site-search>`,
  `<sl-doc-search>`).
- **The gate is "no submittable SEARCH form", not "no forms".** A newsletter form
  (docs.pydantic.dev) or a dead trigger (react.dev) must not silence the note — it's
  suppressed only by a form that could plausibly BE the search (a named text/search field
  with a searchy name or a "search"-labelled field).

### Round four — the honesty seams and the page that talks back

apex1 verified round three 4/4 (including the stale-cache refusal and the mkdocs-material
suppression — the two most regression-likely cases), then filed two seams and a proposal:

- **`submittable` recomputed on read.** Pre-flag cache rows serde-defaulted the flag to
  `true` — stale-optimistic for exactly the pages read before the update, inverting a
  pre-flight flag's purpose. It's derivable from data already in the row, so the cache read
  path now recomputes it from the stored fields every time (shared predicate
  `form_is_submittable`, also used by extraction and the engine's refusal). No migration, no
  refetch; old rows are truthful.
- **The trailer stopped lying.** A dead form's reader-view annotation synthesized
  `submit "Submit"` — prose inviting the exact call `web_submit` refuses (mkdocs.org, where
  the prose is all a reading model sees). Dead forms now annotate as
  `· not submittable (no named fields)`; live forms keep their submit label. Cached markdown
  keeps the old trailer until its next refresh — the registry and the refusal stay truthful
  regardless.
- **`markdown_alternate` reported.** vite.dev serves 10.7 KB of authored markdown
  (`/guide.md`, plus `/llms.txt`) for a page whose SPA HTML is ~150 KB — and says so in its
  own reader view. Per apex1's conservative cut: when a page *advertises* an alternate via
  `<link rel="alternate" type="text/markdown">`, page-ish payloads report it as
  `markdown_alternate` (persisted in the cache, `md_alt` column via the migrate ladder) —
  reported, never auto-followed; the caller chooses. **Parked follow-on:** convention
  probing (same-path `.md`, origin-level `/llms.txt` with a per-origin cached HEAD) — wants
  a second round of field data; vite.dev itself has no `<link>`, so the markup-only cut
  won't reach it yet.

### Round five — the dead alternate, and the prober that deliberately doesn't exist

- **Dead-alternate repair.** docs.deno.com advertises `href="//runtime/index.md"` — RFC 3986
  reads that as a network-path reference (host `runtime`, DNS-dead); the site meant one
  slash. A reported affordance must be real, so resolution now validates the host is
  plausible (dotted, or the page's own — localhost dev pages stay fine): a `//single-label/`
  href is re-read as root-relative against the page's origin (yielding the URL that actually
  serves, verified 200), and an unrepairable implausible host is dropped rather than
  reported. Legitimate cross-origin alternates (hono.dev → raw.githubusercontent.com) pass
  untouched.
- **The `llms.txt` prober stays unbuilt, on field evidence.** apex1's discovery tally
  (5 sites): 4 advertise via `rel=alternate` markup, 1 by prose banner, 0 by bare
  convention. Markup is the majority path, the banner case is legible in the reader view,
  and per-origin probing would double requests to catch it. The tally continues; if the
  ratio inverts, that files the feature.

### Round six — paving the destination

Round five's pointer led somewhere unreadable: `.md` alternates (and `llms.txt`) went
through the **HTML extractor**, which normalizes newlines as insignificant whitespace — but
in markdown newlines ARE the syntax. The deno alternate arrived as one 161-lines-in-1 line,
frontmatter leaked, `title: null`, and `links: []` — an `llms.txt` link index flattened to
prose loses exactly the machine-readable part that makes the convention worth having.

Fix (`extract_response`): a **branch on the declared content type**, not a heuristic.
`text/markdown` / `text/x-markdown` / `text/plain` bodies pass through **verbatim** —
frontmatter `title:` lifted (falling back to the first `# ` heading) and the block
stripped; inline `[t](u)` links and reference definitions collected and resolved against
the fetched URL (root-relative is the norm in authored docs), restoring the registry so
`web_click` traverses `.md` pages and `llms.txt` indexes; `source_format:
"markdown"|"text"` on the payload says which door the text came through (persisted,
`src_fmt` column). HTML and undeclared types keep the extractor path unchanged.

### Round seven — the arc closes

apex1 drove the paved road end-to-end: the deno `.md` arrives structured (title lifted,
25 links, fences intact), and **an `llms.txt` index is now navigable** — `web_click` off
vite's ToC lands on `guide/why.md` through the verbatim branch, a hop that was structurally
impossible when the registry was empty. `src_fmt` confirmed a column, not luck.

- **Legacy flat rows self-heal.** Rows cached before the passthrough hold the old one-line
  rendering with no `source_format` — silently legacy. A NULL-`src_fmt` row whose URL looks
  markdown-ish (`.md` / `.markdown` / `.txt`) is treated as a full cache MISS on next
  access — deliberately not a conditional refresh, since a 304 keeps the flat body (the
  server's content didn't change; our reading did). One live GET per legacy row, once.
  Absent `source_format` on an html-URL row stays by design (the extractor path never sets
  it).
- **By design, not bugs** (apex1's field notes): verbatim means embedded HTML/custom
  elements in a `.md` ride along (`<deno-tabs>` — stripping them would be the extractor
  sneaking back in); and the PROSE keeps the author's relative links while the REGISTRY
  carries them resolved — the prose is the author's, the registry is the machine's, and
  `web_click` uses the machine's.

The alternates arc in full: **discovered** honestly (#14) → **repaired** when broken (#15)
→ **readable** on arrival (#16) → **healed** in the cache (this) — four rounds, one
coherent capability, every step driven by walking the road rather than reasoning about it.

## The JS door

### What a real engine buys that the trick can't

| Capability | Static + salvage (Nano and up) | Render sidecar (opt-in, heavy nodes) |
|---|---|---|
| Static/SSR pages (most public content) | ✅ | ✅ (wasteful) |
| State-blob SPAs (Next/Nuxt/Redux/ld+json) | ✅ salvage | ✅ |
| HTML forms, links, cookies, multi-step flows | ✅ Phases 12–15 | ✅ |
| Client-only rendered apps (content arrives via XHR after load: dashboards, Grafana-style tools) | ❌ `js_required: true` | ✅ |
| JS-wired widgets (onclick-only buttons, infinite scroll, dynamic dropdowns) | ❌ | ✅ |
| JS-computed form tokens (beyond hidden inputs) | ❌ | ✅ |
| WebSocket/live-updating content | ❌ | ✅ |
| RAM cost | ~unchanged | ~200–500 MB+ per browser instance |
| Attack surface | none added (no script execution) | the browser's (sandboxed, patched upstream) |
| Politeness | 1 request = 1 request | 1 page load = dozens of subresource requests to budget as one unit |

For *reading and research* — Occipital's actual job — static + salvage covers the large
majority of the public web, precisely because content that wants to be found server-renders or
embeds its state. The sidecar's win is *operating web apps*, which is a different (and much
rarer) need.

### The door's contract

- **`PageLoader` seam** — the load step goes behind a small trait
  (`load(url) → RawPage { html, final_url, … }`). `StaticLoader` (= today's `PoliteFetcher`
  path) is the only in-tree impl and the default forever.
- **Abstract verbs** — click/submit address element ordinals, not pixels, so a render backend
  can execute them *for real* while the static backend does href/form semantics. The tool
  surface never changes with the backend.
- **Escalation ladder** — cache → static fetch → salvage → *(if enabled)* render. `js_required`
  is the escalation trigger. Nano stops at salvage; a heavy node can walk the whole ladder.
- **The occupant is a sidecar, not an embed.** If/when built (separate project, e.g.
  `occipital-render`): a feature-gated component driving a **system-installed headless
  browser over CDP as a client** (e.g. `chromiumoxide`) — mirroring the embeddings pattern
  (build feature + env var `OCCIPITAL_RENDER_BACKEND`, default `off`, zero trace in the
  default build). An **in-process JS engine (Boa etc.) is permanently rejected**: executing
  the language without the web platform renders ~nothing real, building the platform is
  Servo, and running untrusted scripts in-process with no sandbox is a security hole a real
  browser's sandbox already solves.

## Env vars (planned by these phases)

| Var | Default | Phase | Purpose |
|-----|---------|-------|---------|
| `OCCIPITAL_SNAPSHOT_TTL_SECS` | `3600` | 12 | interaction-snapshot retention |
| `OCCIPITAL_COOKIES` | `0` | 15 | enable the persistent cookie jar |
| `OCCIPITAL_COOKIES_FILE` | `<data_dir>/occipital/cookies.json` | 15 | jar path (0600) |
| `OCCIPITAL_PROXY` | unset | 15 | explicit proxy URL (logged at startup) |
| `OCCIPITAL_HEADERS_FILE` | unset | 15 | per-domain extra-header map (UA not overridable) |
| `OCCIPITAL_ROBOTS_TTL_SECS` | `3600` | 16 | robots.txt re-check interval |
| `OCCIPITAL_RENDER_BACKEND` | `off` | door | future render sidecar selector |

## Out of scope — permanently

In-process JS engine · CDP **server** · CAPTCHA solving · paywall/login-wall bypass ·
fingerprint/UA spoofing · cookie/session farming or rotation · credential harvesting.
The politeness contract ([politeness.md](politeness.md)) governs every phase above; where this
doc and that one seem to tension, politeness wins.
