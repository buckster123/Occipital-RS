//! # Occipital — the agent's reading cortex
//!
//! A pure-Rust web layer: a polite fetcher, reader-mode extraction, pluggable
//! search providers, and a Cerebro-style decaying knowledge cache. Three thin
//! binaries (`occipital-mcp` / `-api` / `-cli`) drive this one library.
//!
//! Build status: **Phase 12 — agent browsing: the interactive page model**.
//! Phases 0–11 complete (standalone, the ApexOS follow-along UI, the knowledge
//! hub); [`extract`] now carries the page's interactive surface (forms, with
//! stable ordinals) and the cache holds TTL-bounded raw-HTML snapshots for the
//! coming interaction verbs — see `docs/agent-browsing.md` + `docs/build-roadmap.md`.

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

pub use cache::{Cache, CacheStats};
pub use config::{Config, Tier};
pub use curate::{
    make_auto_distiller, make_distiller, AutoDistill, CurateBackend, CurateConfig, Distillation,
    Distiller,
};
pub use embed::{cosine, make_embedder, Embedder};
pub use engine::{DomView, Engine, IndexedLink, RecallHit};
pub use keys::Keys;
pub use extract::{extract, extract_bytes, Form, FormField, Link, Page};
pub use fetch::{FetchResponse, Fetcher, HttpRequest, Method, PoliteFetcher};
pub use providers::{SearchProvider, SearchResult};

/// The crate version (the `serverInfo.version` an MCP client sees).
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
