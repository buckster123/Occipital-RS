//! # Occipital — the agent's reading cortex
//!
//! A pure-Rust web layer: a polite fetcher, reader-mode extraction, pluggable
//! search providers, and a Cerebro-style decaying knowledge cache. Three thin
//! binaries (`occipital-mcp` / `-api` / `-cli`) drive this one library.
//!
//! Build status: **Phase 14 — agent browsing: SPA salvage + honest JS signaling**.
//! Phases 0–13 complete (standalone, the ApexOS follow-along UI, the knowledge
//! hub, the page model, the interaction verbs); thin pages now mine the data
//! embedded in static HTML (`salvaged: true`) and truly client-only pages say
//! so (`js_required: true`) — no script is ever executed. See
//! `docs/agent-browsing.md` + `docs/build-roadmap.md`.

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

/// The crate version (the `serverInfo.version` an MCP client sees).
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
