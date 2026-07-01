//! Runtime configuration — env + sensible, **polite** defaults.
//!
//! Every politeness knob is tunable, but the *defaults* stay conservative (see
//! `docs/politeness.md`): a node is a good web citizen out of the box. The tier
//! (Nano vs Micro+) is derived from whether an embedding model is configured,
//! exactly like Cerebro.

use std::env;
use std::path::PathBuf;

use crate::curate::CurateConfig;

/// Default embedding model (Micro+ tier). Empty string → FTS5-only (Nano).
pub const EMBED_MODEL: &str = "BAAI/bge-small-en-v1.5";

/// Polite-by-default fetch knobs (see `docs/politeness.md`).
pub const DEFAULT_RATE_PER_DOMAIN: f64 = 0.5; // requests/sec per registrable domain
pub const DEFAULT_MAX_CONCURRENCY: usize = 4; // global in-flight fetches
pub const DEFAULT_FRESH_TTL_SECS: u64 = 86_400; // cache freshness window
pub const DEFAULT_FETCH_TIMEOUT_SECS: u64 = 30; // Nano-safe per-request timeout
pub const DEFAULT_MAX_BODY_BYTES: usize = 2_000_000; // per-page body cap
pub const DEFAULT_SEARCH_TOP_N: usize = 5; // results fetched per search
pub const DEFAULT_SEARCH_PROVIDER: &str = "duckduckgo";
pub const DEFAULT_MAX_RETRIES: u32 = 3; // polite backoff retries on 429/503/transient

// Decay / forgetting (Phase 6). Web content perishes faster than lived memory,
// so the half-life is short by default. A cached page's standing halves every
// `half_life` of *disuse*; below the GC floor (and unpinned, past min-age) it is
// pruned — keeping recall sharp instead of flooded with stale pages.
pub const DEFAULT_DECAY_HALFLIFE_SECS: u64 = 604_800; // 7 days
pub const DEFAULT_GC_MIN_SALIENCE: f32 = 0.15; // prune below this effective salience
pub const DEFAULT_GC_MIN_AGE_SECS: u64 = 86_400; // never GC a page fetched < 1 day ago

// Compile-time guard: a conservative posture is the whole point — a careless
// edit that makes the crawler aggressive must fail the build, not just a test.
const _: () = assert!(DEFAULT_RATE_PER_DOMAIN <= 1.0, "default ≤ 1 req/s per domain");
const _: () = assert!(DEFAULT_MAX_CONCURRENCY <= 8, "bounded global concurrency");
const _: () = assert!(DEFAULT_FETCH_TIMEOUT_SECS >= 30, "Nano-safe fetch timeout");

/// Hardware/capability tier — mirrors Cerebro / ApexOS. The web layer behaves
/// identically across tiers; faster tiers just gain semantic recall.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// No embeddings — FTS5 keyword cache only. ~no extra RSS.
    Nano,
    /// bge-small embeddings — cosine semantic recall over the cache.
    Micro,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub db_path:            PathBuf,
    pub keys_file:          PathBuf,
    /// Empty string → FTS5-only (Nano); otherwise the bge model id (Micro+).
    pub embed_model:        String,
    /// Honest, identifiable user-agent. Never randomized per-request.
    pub user_agent:         String,
    pub respect_robots:     bool,
    pub rate_per_domain:    f64,
    pub max_concurrency:    usize,
    pub fresh_ttl_secs:     u64,
    pub fetch_timeout_secs: u64,
    pub max_body_bytes:     usize,
    pub search_top_n:       usize,
    pub search_provider:    String,
    pub searxng_url:        Option<String>,
    pub max_retries:        u32,
    pub decay_half_life_secs: u64,
    pub gc_min_salience:    f32,
    pub gc_min_age_secs:    u64,
    /// LLM curation (the distillation layer) — see `curate`.
    pub curate:             CurateConfig,
}

impl Config {
    /// Assemble config from the environment, falling back to polite defaults.
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            db_path:            db_path(),
            keys_file:          keys_file(),
            embed_model:        env::var("OCCIPITAL_EMBED_MODEL")
                                    .unwrap_or_else(|_| EMBED_MODEL.to_string()),
            user_agent:         env::var("OCCIPITAL_USER_AGENT")
                                    .unwrap_or_else(|_| default_user_agent()),
            respect_robots:     env_bool("OCCIPITAL_RESPECT_ROBOTS", true),
            rate_per_domain:    env_parse("OCCIPITAL_RATE_PER_DOMAIN", DEFAULT_RATE_PER_DOMAIN),
            max_concurrency:    env_parse("OCCIPITAL_MAX_CONCURRENCY", DEFAULT_MAX_CONCURRENCY),
            fresh_ttl_secs:     env_parse("OCCIPITAL_FRESH_TTL_SECS", DEFAULT_FRESH_TTL_SECS),
            fetch_timeout_secs: env_parse("OCCIPITAL_FETCH_TIMEOUT_SECS", DEFAULT_FETCH_TIMEOUT_SECS),
            max_body_bytes:     env_parse("OCCIPITAL_MAX_BODY_BYTES", DEFAULT_MAX_BODY_BYTES),
            search_top_n:       env_parse("OCCIPITAL_SEARCH_TOP_N", DEFAULT_SEARCH_TOP_N),
            search_provider:    env::var("OCCIPITAL_SEARCH_PROVIDER")
                                    .unwrap_or_else(|_| DEFAULT_SEARCH_PROVIDER.to_string()),
            searxng_url:        env::var("OCCIPITAL_SEARXNG_URL").ok().filter(|s| !s.is_empty()),
            max_retries:        env_parse("OCCIPITAL_MAX_RETRIES", DEFAULT_MAX_RETRIES),
            decay_half_life_secs: env_parse("OCCIPITAL_DECAY_HALFLIFE_SECS", DEFAULT_DECAY_HALFLIFE_SECS),
            gc_min_salience:    env_parse("OCCIPITAL_GC_MIN_SALIENCE", DEFAULT_GC_MIN_SALIENCE),
            gc_min_age_secs:    env_parse("OCCIPITAL_GC_MIN_AGE_SECS", DEFAULT_GC_MIN_AGE_SECS),
            curate:             CurateConfig::from_env(),
        })
    }

    /// Nano when no embedding model is configured, Micro+ otherwise.
    pub fn tier(&self) -> Tier {
        if self.embed_model.is_empty() { Tier::Nano } else { Tier::Micro }
    }
}

/// The default honest user-agent: says what it is and where to find it.
fn default_user_agent() -> String {
    format!(
        "Occipital/{} (+https://github.com/buckster123/Occipital-RS; ApexOS web reader)",
        env!("CARGO_PKG_VERSION"),
    )
}

/// SQLite path: `OCCIPITAL_DB`, else `<data_dir>/occipital/occipital.db`.
fn db_path() -> PathBuf {
    if let Ok(p) = env::var("OCCIPITAL_DB") {
        return PathBuf::from(p);
    }
    occipital_dir().join("occipital.db")
}

/// Provider key store: `OCCIPITAL_KEYS_FILE`, else `<data_dir>/occipital/keys.json`.
fn keys_file() -> PathBuf {
    if let Ok(p) = env::var("OCCIPITAL_KEYS_FILE") {
        return PathBuf::from(p);
    }
    occipital_dir().join("keys.json")
}

fn occipital_dir() -> PathBuf {
    dirs_next::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("occipital")
}

fn env_bool(key: &str, default: bool) -> bool {
    match env::var(key) {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"),
        Err(_) => default,
    }
}

fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(embed: &str) -> Config {
        Config {
            db_path:            PathBuf::from("/tmp/x.db"),
            keys_file:          PathBuf::from("/tmp/keys.json"),
            embed_model:        embed.to_string(),
            user_agent:         default_user_agent(),
            respect_robots:     true,
            rate_per_domain:    DEFAULT_RATE_PER_DOMAIN,
            max_concurrency:    DEFAULT_MAX_CONCURRENCY,
            fresh_ttl_secs:     DEFAULT_FRESH_TTL_SECS,
            fetch_timeout_secs: DEFAULT_FETCH_TIMEOUT_SECS,
            max_body_bytes:     DEFAULT_MAX_BODY_BYTES,
            search_top_n:       DEFAULT_SEARCH_TOP_N,
            search_provider:    DEFAULT_SEARCH_PROVIDER.to_string(),
            searxng_url:        None,
            max_retries:        DEFAULT_MAX_RETRIES,
            decay_half_life_secs: DEFAULT_DECAY_HALFLIFE_SECS,
            gc_min_salience:    DEFAULT_GC_MIN_SALIENCE,
            gc_min_age_secs:    DEFAULT_GC_MIN_AGE_SECS,
            curate:             CurateConfig::default(),
        }
    }

    #[test]
    fn empty_embed_model_is_nano() {
        assert_eq!(cfg("").tier(), Tier::Nano);
    }

    #[test]
    fn embed_model_set_is_micro() {
        assert_eq!(cfg(EMBED_MODEL).tier(), Tier::Micro);
    }

    #[test]
    fn user_agent_is_honest_and_identifiable() {
        let ua = default_user_agent();
        assert!(ua.starts_with("Occipital/"), "names itself: {ua}");
        assert!(ua.contains("github.com/buckster123/Occipital-RS"), "says where to find it: {ua}");
    }
    // The polite-defaults invariant is now a compile-time guard (see the
    // `const _: () = assert!(...)` block above the `Config` definition).
}
