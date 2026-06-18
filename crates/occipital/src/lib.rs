//! # Occipital — the agent's reading cortex
//!
//! A pure-Rust web layer: a polite fetcher, reader-mode extraction, pluggable
//! search providers, and a Cerebro-style decaying knowledge cache. Three thin
//! binaries (`occipital-mcp` / `-api` / `-cli`) drive this one library.
//!
//! Build status: **Phase 7 — keyed providers**. All core modules live, plus
//! [`keys`] + Brave/Tavily/Bing providers. Remaining: the API/CLI surfaces
//! (Phase 8) and the ApexOS follow-along UI (Phase 9). See `docs/build-roadmap.md`.

pub mod cache;
pub mod config;
pub mod decay;
pub mod embed;
pub mod engine;
pub mod extract;
pub mod fetch;
pub mod keys;
pub mod providers;
pub mod ratelimit;
pub mod robots;

pub use cache::Cache;
pub use config::{Config, Tier};
pub use embed::{cosine, make_embedder, Embedder};
pub use engine::{Engine, RecallHit};
pub use keys::Keys;
pub use extract::{extract, extract_bytes, Link, Page};
pub use fetch::{FetchResponse, Fetcher, HttpRequest, Method, PoliteFetcher};
pub use providers::{SearchProvider, SearchResult};

/// The crate version (the `serverInfo.version` an MCP client sees).
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
