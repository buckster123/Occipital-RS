//! # Occipital — the agent's reading cortex
//!
//! A pure-Rust web layer: a polite fetcher, reader-mode extraction, pluggable
//! search providers, and a Cerebro-style decaying knowledge cache. Three thin
//! binaries (`occipital-mcp` / `-api` / `-cli`) drive this one library.
//!
//! Build status: **Phase 6 — decay & forgetting**. [`config`], [`ratelimit`],
//! [`robots`], [`fetch`], [`extract`], [`providers`], [`cache`], [`embed`],
//! [`decay`], and [`engine`] are live; the remaining phases are keyed providers,
//! the API/CLI surfaces, and the ApexOS UI. See `docs/build-roadmap.md`.

pub mod cache;
pub mod config;
pub mod decay;
pub mod embed;
pub mod engine;
pub mod extract;
pub mod fetch;
pub mod providers;
pub mod ratelimit;
pub mod robots;

pub use cache::Cache;
pub use config::{Config, Tier};
pub use embed::{cosine, make_embedder, Embedder};
pub use engine::{Engine, RecallHit};
pub use extract::{extract, extract_bytes, Link, Page};
pub use fetch::{FetchResponse, Fetcher, PoliteFetcher};
pub use providers::{SearchProvider, SearchResult};

/// The crate version (the `serverInfo.version` an MCP client sees).
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
