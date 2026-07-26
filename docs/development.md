# Occipital-RS — Development

Everything a contributor (human or agent) needs that doesn't belong in the README. The README
is the product face; this is the workshop.

> Conventions inherited from the sibling repos (`CerebroCortex-RS`, `ApexOS-RS`): clippy-clean
> always, tests per crate, docs travel with code, feature branch → PR, never commit to `main`.

---

## Prerequisites

Stable Rust (via [rustup](https://rustup.rs)) and a C toolchain for `rusqlite`'s bundled
SQLite. Nothing else — no Node, no Python, no browser, and no model download on the default
build.

## Building

```bash
cargo build --release                      # all four crates (Nano: no ONNX runtime)
cargo build --release -p occipital-mcp     # just the MCP server
cargo build --release --features embeddings   # Micro+: pulls in fastembed/ONNX
```

The release profile is `opt-level = 3`, thin LTO, stripped. Reference size on x86-64:
`occipital-mcp` ≈ **8.3 MB** stripped on the default build.

### The two tiers

Embeddings are an **opt-in build feature**, not just a runtime toggle — the default build
excludes the ONNX runtime entirely so a Nano-class node stays small and needs no build-time
download.

| | Feature flag | `OCCIPITAL_EMBED_MODEL` | Recall path |
|---|---|---|---|
| **Nano** | *(none)* | ignored (warned if set) | FTS5 keyword |
| **Micro+** | `--features embeddings` | `BAAI/bge-small-en-v1.5` | cosine semantic, FTS5 fallback |

Design rule inherited from ApexOS: **build for Nano first** — graceful when embeddings are off,
no timeouts shorter than 30 s, never assume a key is present.

## Testing

```bash
cargo test --workspace                     # 141 tests, no network, no model download
cargo test --workspace --features occipital/embeddings
cargo test -p occipital -- --ignored       # the one live example.com fetch
```

| Crate | Tests | What they gate |
|-------|-------|----------------|
| `occipital` (lib) | 127 | everything below |
| `occipital-mcp` | 14 | dispatch routing, tool contracts, payload shapes, panic isolation |
| **total** | **141** | |

Inside the library, by module:

| Module | Tests | Focus |
|--------|-------|-------|
| `engine` | 24 | read-through cache, recall ranking, distill sweep + budget, click/submit semantics, GC |
| `extract` | 16 | reader-mode conversion, form/element registry, salvage + `js_required` flags |
| `cache` | 14 | page/snapshot/distillation round-trips, FTS5, schema migration, request log bounds |
| `curate` | 11 | prompt building, response parsing, backend tiering |
| `ratelimit` | 10 | spacing floors, additive jitter, backoff, `Crawl-delay` overrides, eTLD+1 buckets |
| `providers` | 10 | per-provider request building + parsing against saved fixtures |
| `session` | 9 | cookie matching/expiry/persistence, header rules, UA lock |
| `salvage` | 8 | `ld+json`, state blobs, `noscript`, harvest bounds |
| `fetch` | 7 | UA token, rate keys, robots URL (port!), POST non-retry |
| `robots` | 6 | group selection, longest-match, wildcards |
| `decay`, `keys`, `config`, `embed` | 13 | pure math, key store, tier detection, cosine |

**Network paths are behind the `Fetcher` trait.** Every test above runs offline against mocks
or fixtures; the single live test is `#[ignore]`d. Politeness math (spacing, jitter, backoff,
decay) is pure and unit-tested without a clock.

## Lint gate

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --features occipital/embeddings -- -D warnings
```

Zero warnings, both tiers, workspace-wide. CI runs the same two commands.

## Verifying against the real web

Unit tests prove logic; a **live gate** proves the thing works. Every phase in
[build-roadmap.md](build-roadmap.md) records one. Two techniques worth reusing:

**A local origin for politeness behavior.** Serving a fixture site is the only honest way to
test robots/`Crawl-delay` without hammering someone:

```bash
mkdir -p /tmp/site && cd /tmp/site
printf 'User-agent: *\nCrawl-delay: 5\nDisallow: /private/\n' > robots.txt
echo '<html><body><main><p>hello</p></main></body></html>' > a.html
python3 -m http.server 8791 --bind 127.0.0.1 &

OCCIPITAL_DB=/tmp/t.db occipital fetch http://127.0.0.1:8791/a.html
OCCIPITAL_DB=/tmp/t.db occipital log --limit 3     # shows the honored 5s wait
```

This is exactly how the Phase 16 gate caught a real bug: `host_str()` drops the port, so
robots.txt for any origin not on 80/443 had been fetched from the default port and fell back to
allow-all. Fixtures would never have found it.

**Live surfaces through the CLI.** `occipital dom`/`click`/`submit` against a real page (a
search form is ideal) exercises the whole pipeline — politeness gate, extraction, element
registry, cache — in one command.

## Repo layout

```
crates/
  occipital/        # the library
    fetch.rs        # the only network door — politeness, robots, cookies, headers, request log seam
    ratelimit.rs    # spacing math, eTLD+1 buckets, Crawl-delay overrides
    robots.rs       # robots.txt parser
    extract.rs      # HTML → Markdown + links + the element registry (forms)
    salvage.rs      # no-JS SPA recovery + js_required signaling
    providers.rs    # SearchProvider trait + 5 impls
    cache.rs        # SQLite: pages, searches, embeddings, distillations, snapshots, requests
    decay.rs        # salience math
    embed.rs        # Embedder trait + feature-gated fastembed
    curate.rs       # LLM distillation (tiered Ollama → Anthropic)
    session.rs      # cookie jar + per-domain header rules
    engine.rs       # the verbs every binary drives
    config.rs       # env → Config, tier detection
    keys.rs         # 0600 provider key store
  occipital-mcp/    # newline-delimited JSON-RPC over stdio (no SDK), protocol 2024-11-05
  occipital-api/    # axum REST
  occipital-cli/    # clap CLI
docs/               # design docs — the planning lock-in
```

## House rules

- **Clippy-clean, always.** Zero warnings is the gate, not a goal.
- **Tests per crate, built incrementally.** Pure logic unit-tested without network; network
  paths behind `Fetcher` + a mock.
- **Fail soft, never silently.** A parser that breaks returns empty results and logs; it never
  panics and never fabricates. An unavailable backend produces an honest error.
- **Politeness is non-negotiable.** Defaults stay conservative even when a knob exists. A
  change that makes the crawler more aggressive by default should fail the build — see the
  compile-time asserts in `config.rs`.
- **Docs travel with code.** Update `CLAUDE.md` + the relevant `docs/*.md` in the same PR.
- **Git**: feature branch → PR, never commit to `main`. End commits with the `Co-Authored-By`
  trailer.

## Adding an MCP tool

1. Add the name to `TOOL_NAMES` in `crates/occipital-mcp/src/tools.rs` (the single source of
   truth) and give it a schema in the same file.
2. Add the handler arm in `dispatch.rs`, returning a flat `kind`-discriminated object — it is
   both the agent payload and the UI render payload (see [follow-along.md](follow-along.md)).
3. Implement the verb on `Engine` so the CLI and REST surfaces can share it.
4. Add dispatch tests; extend the CLI/API for parity.
5. Update the tool table in `README.md` + `CLAUDE.md`, and note it in `build-roadmap.md`.

## Storage schema changes

`CREATE TABLE IF NOT EXISTS` never alters an existing table, so **additive columns need a
migration entry** — see the `ADDED` table in `cache.rs::migrate()`. Add the column to both the
`SCHEMA` const (for fresh DBs) and that list (for existing ones), and cover it with a test that
opens a hand-built old-schema DB. Live nodes upgrade in place on next open.
