//! # Occipital — the agent's reading cortex
//!
//! A pure-Rust web layer: a polite fetcher, reader-mode extraction, pluggable
//! search providers, and a Cerebro-style decaying knowledge cache. Three thin
//! binaries (`occipital-mcp` / `-api` / `-cli`) drive this one library.
//!
//! Build status: **Phase 16 — the agent-browsing expansion is complete**.
//! Phases 0–16: the polite fetcher + reader-mode + cache + decay + knowledge
//! hub, then browsing — the interactive page model, `click`/`submit`, SPA
//! salvage, sessions & identity, and multi-step politeness hygiene (robots
//! `Crawl-delay` honored, eTLD+1 rate buckets, TTL'd robots cache, a bounded
//! request log). See `docs/agent-browsing.md` + `docs/politeness.md`.

pub mod cache;
pub mod config;
pub mod curate;
pub mod decay;
pub mod embed;
pub mod engine;
pub mod extract;
pub mod fetch;
pub mod keys;
pub mod providers;
pub mod ratelimit;
pub mod robots;
mod salvage;
pub mod session;

pub use cache::{Cache, CacheStats};
pub use config::{Config, Tier};
pub use curate::{
    make_auto_distiller, make_distiller, AutoDistill, CurateBackend, CurateConfig, Distillation,
    Distiller,
};
pub use embed::{cosine, make_embedder, Embedder};
pub use engine::{ClickReport, DomView, Engine, IndexedLink, RecallHit, SentField, SubmitReport};
pub use keys::Keys;
pub use extract::{extract, extract_bytes, Form, FormField, Link, Page};
pub use fetch::{FetchResponse, Fetcher, HttpRequest, Method, PoliteFetcher};
pub use providers::{SearchProvider, SearchResult};
pub use session::{Cookie, CookieJar, HeaderRules};

/// The crate version (the `serverInfo.version` an MCP client sees).
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
