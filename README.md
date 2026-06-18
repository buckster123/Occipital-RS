# Occipital-RS

**The agent's reading cortex.** A pure-Rust web layer that gives an AI polite eyes on the live
web and a decaying memory of what it read — search, fetch, reader-mode extraction, and a
Cerebro-style knowledge cache, behind **MCP / CLI / REST**.

If [Cerebro](../CerebroCortex-RS) is episodic memory, Occipital is the visual/reading cortex —
what the agent *sees and reads* of the world outside itself.

## Why

LLM agents with a raw `curl`-style tool are bad web citizens: they fire off suspicious request
volumes, get rate-limited or blocked, and dump unreadable HTML into their own context. Occipital
fixes both ends:

- **Polite by construction** — per-domain rate limiting, jitter, backoff, robots-awareness, an
  honest user-agent. Good citizenship, not cloaking.
- **Readable** — every page comes back as clean Markdown (reader-mode), so the agent reasons
  over signal, not navigation chrome — and a human can read the exact same view.
- **Remembered** — pages are cached, embedded, and **decay over time**. A search hits the local
  cache first; only a miss goes live. Queryable like Cerebro, with forgetting so stale pages
  don't flood results.

## Shape

```
crates/occipital       core lib (fetch · politeness · extract · providers · cache · decay)
crates/occipital-mcp   stdio JSON-RPC MCP server
crates/occipital-api   axum REST (management + query)
crates/occipital-cli   clap CLI (ops, provider keys, queries)
```

Standalone-first: adopt it like Cerebro, with zero ApexOS dependency.

## Status

📐 **Design / planning.** See [CLAUDE.md](CLAUDE.md) and [`docs/`](docs/). Build roadmap:
[docs/build-roadmap.md](docs/build-roadmap.md).

## License

TBD (match the ApexOS / Cerebro ecosystem).
