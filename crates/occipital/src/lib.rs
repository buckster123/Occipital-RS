//! # Occipital — the agent's reading cortex
//!
//! A pure-Rust web layer: a polite fetcher, reader-mode extraction, pluggable
//! search providers, and a Cerebro-style decaying knowledge cache. Three thin
//! binaries (`occipital-mcp` / `-api` / `-cli`) drive this one library.
//!
//! Build status: **Phase 0 — scaffold**. Only [`config`] is substantive so far;
//! the pipeline modules (`fetch`, `extract`, `providers`, `cache`, `decay`,
//! `rank`) land in their roadmap phases. See `docs/build-roadmap.md`.

pub mod config;

pub use config::{Config, Tier};

/// The crate version (the `serverInfo.version` an MCP client sees).
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
