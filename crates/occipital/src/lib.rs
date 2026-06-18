//! # Occipital — the agent's reading cortex
//!
//! A pure-Rust web layer: a polite fetcher, reader-mode extraction, pluggable
//! search providers, and a Cerebro-style decaying knowledge cache. Three thin
//! binaries (`occipital-mcp` / `-api` / `-cli`) drive this one library.
//!
//! Build status: **Phase 2 — reader-mode**. [`config`], [`ratelimit`],
//! [`robots`], [`fetch`], and [`extract`] are live; the rest (`providers`,
//! `cache`, `decay`, `rank`) land in their roadmap phases. See
//! `docs/build-roadmap.md`.

pub mod config;
pub mod extract;
pub mod fetch;
pub mod ratelimit;
pub mod robots;

pub use config::{Config, Tier};
pub use extract::{extract, extract_bytes, Link, Page};
pub use fetch::{FetchResponse, Fetcher, PoliteFetcher};

/// The crate version (the `serverInfo.version` an MCP client sees).
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
