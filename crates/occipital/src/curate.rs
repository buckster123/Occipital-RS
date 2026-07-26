//! LLM curation — the distillation layer of the knowledge hub.
//!
//! A raw cached page is a *page*; a distilled page is *knowledge*: a short
//! summary, the key points it asserts, the entities it mentions, and topic
//! tags. Distillation is what turns the read-through cache from "a pile of
//! markdown I once fetched" into something the agent can recall and reason
//! over — `web_recall` serves the distilled summary (not a raw-body snippet)
//! and the tags/entities become keyword-findable even on Nano.
//!
//! The LLM transport mirrors Cerebro's `describe_image` tiering (one tool, two
//! transports, three deployment shapes):
//!
//! | Tier | Hardware                | Transport            |
//! |------|-------------------------|----------------------|
//! | a    | small LLM on the node   | Ollama @ localhost   |
//! | b    | a LAN inference node    | Ollama @ a LAN URL   ← same transport as (a) |
//! | c    | external API            | Anthropic (haiku)    |
//!
//! Backend is selected by `OCCIPITAL_CURATE_BACKEND` (`auto`|`ollama`|
//! `anthropic`|`off`). `auto` (default) prefers a reachable Ollama, falls back
//! to Anthropic when `ANTHROPIC_API_KEY` is set, else returns an honest "no
//! backend" error. Distillation only runs when explicitly asked (`web_distill`
//! / CLI / API) — nothing spends tokens behind the operator's back.
//!
//! This module talks to an LLM endpoint (localhost Ollama or api.anthropic.com),
//! **not** the open web — so it deliberately uses a plain `reqwest` client, not
//! the polite [`Fetcher`](crate::fetch::Fetcher) (robots/rate-limiting are
//! scraping etiquette; an inference API is a consented service).

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::extract::Page;

/// Default Ollama endpoint — covers node-local **and** a LAN inference node
/// (just change the URL).
const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";
/// Default Ollama curation model — small, text-only, widely pulled.
const DEFAULT_OLLAMA_MODEL: &str = "llama3.2";
/// Cheapest Anthropic model that distills well.
const DEFAULT_ANTHROPIC_MODEL: &str = "claude-haiku-4-5";
/// Body cap fed to the model (chars) — bounds cost per page.
const MAX_BODY_CHARS: usize = 8_000;
/// Bounds on the parsed output (storage sanity; a rambling model can't bloat rows).
const MAX_SUMMARY_CHARS: usize = 1_200;
const MAX_KEY_POINTS: usize = 10;
const MAX_ENTITIES: usize = 20;
const MAX_TAGS: usize = 8;

/// Auto-distillation defaults — the "living" knob (see [`AutoDistill`]).
const DEFAULT_AUTO_INTERVAL_SECS: u64 = 300;
const DEFAULT_AUTO_CAP: usize = 50;

/// Whether pages distill themselves in the background (the resident servers'
/// periodic sweep). **Off by default** — auto-curation is opt-in, and `local`
/// exists so a node can keep auto strictly on the free Ollama path even when
/// explicit `web_distill` calls may fall back to the paid API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoDistill {
    /// No background curation (default). `web_distill` stays explicit-only.
    Off,
    /// Background curation via Ollama ONLY — never spends API tokens.
    Local,
    /// Background curation via the configured backend (API fallback included).
    On,
}

impl AutoDistill {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "local" | "ollama" => Self::Local,
            "on" | "1" | "true" | "yes" | "any" => Self::On,
            _ => Self::Off,
        }
    }
}

/// Which transport distillation uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CurateBackend {
    /// Prefer a reachable Ollama, fall back to Anthropic.
    Auto,
    /// Local or LAN Ollama only.
    Ollama,
    /// External Anthropic API only.
    Anthropic,
    /// Curation disabled — `web_distill` returns an honest error.
    Off,
}

impl CurateBackend {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "ollama" | "lan" | "local" => Self::Ollama,
            "anthropic" | "api" | "external" => Self::Anthropic,
            "off" | "none" | "0" | "false" | "disabled" => Self::Off,
            _ => Self::Auto,
        }
    }
}

/// Resolved curation configuration (env-read once at engine build).
#[derive(Debug, Clone)]
pub struct CurateConfig {
    pub backend: CurateBackend,
    /// Ollama base URL (no trailing slash). Point at a LAN node for tier (b).
    pub ollama_url: String,
    pub ollama_model: String,
    pub anthropic_key: Option<String>,
    pub anthropic_model: String,
    /// Background curation mode (default off — see [`AutoDistill`]).
    pub auto: AutoDistill,
    /// Seconds between background sweep ticks (floor 30).
    pub auto_interval_secs: u64,
    /// Max distillations per rolling 24 h before auto pauses (0 = uncapped).
    /// Counts ALL distillations (explicit included) — a total-spend guard.
    pub auto_cap: usize,
}

impl Default for CurateConfig {
    fn default() -> Self {
        Self {
            backend: CurateBackend::Auto,
            ollama_url: DEFAULT_OLLAMA_URL.to_string(),
            ollama_model: DEFAULT_OLLAMA_MODEL.to_string(),
            anthropic_key: None,
            anthropic_model: DEFAULT_ANTHROPIC_MODEL.to_string(),
            auto: AutoDistill::Off,
            auto_interval_secs: DEFAULT_AUTO_INTERVAL_SECS,
            auto_cap: DEFAULT_AUTO_CAP,
        }
    }
}

impl CurateConfig {
    /// Read backend selection + endpoints from the environment.
    pub fn from_env() -> Self {
        let backend = std::env::var("OCCIPITAL_CURATE_BACKEND")
            .map(|s| CurateBackend::parse(&s))
            .unwrap_or(CurateBackend::Auto);
        let ollama_url = std::env::var("OCCIPITAL_CURATE_URL")
            .unwrap_or_else(|_| DEFAULT_OLLAMA_URL.to_string());
        let ollama_model = std::env::var("OCCIPITAL_CURATE_MODEL")
            .unwrap_or_else(|_| DEFAULT_OLLAMA_MODEL.to_string());
        let anthropic_key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|k| !k.trim().is_empty());
        let anthropic_model = std::env::var("OCCIPITAL_CURATE_API_MODEL")
            .ok()
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_ANTHROPIC_MODEL.to_string());
        let auto = std::env::var("OCCIPITAL_AUTO_DISTILL")
            .map(|s| AutoDistill::parse(&s))
            .unwrap_or(AutoDistill::Off);
        let env_parse = |key: &str, default: u64| -> u64 {
            std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
        };
        Self {
            backend,
            ollama_url: ollama_url.trim_end_matches('/').to_string(),
            ollama_model,
            anthropic_key,
            anthropic_model,
            auto,
            auto_interval_secs: env_parse("OCCIPITAL_AUTO_DISTILL_INTERVAL_SECS", DEFAULT_AUTO_INTERVAL_SECS).max(30),
            auto_cap: env_parse("OCCIPITAL_AUTO_DISTILL_CAP", DEFAULT_AUTO_CAP as u64) as usize,
        }
    }
}

/// A distilled page: the curated knowledge stored beside the raw cache row.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Distillation {
    /// 2–4 sentences of what the page says (facts, not meta-description).
    pub summary: String,
    /// The concrete claims/facts the page asserts.
    pub key_points: Vec<String>,
    /// Proper nouns worth indexing (people, orgs, products, standards).
    pub entities: Vec<String>,
    /// Lowercase topic tags.
    pub tags: Vec<String>,
    /// Which model produced it (provenance).
    pub model: String,
    /// Which transport answered (`ollama` / `anthropic`).
    pub backend: &'static str,
}

/// The distillation transport. A trait so the engine is testable without an
/// LLM — mirrors [`Fetcher`](crate::fetch::Fetcher) / [`Embedder`](crate::embed::Embedder).
#[async_trait]
pub trait Distiller: Send + Sync {
    async fn distill_page(&self, page: &Page) -> Result<Distillation>;
}

/// Build the production distiller, or `None` when curation is off.
pub fn make_distiller(cfg: &CurateConfig) -> Option<std::sync::Arc<dyn Distiller>> {
    match cfg.backend {
        CurateBackend::Off => None,
        _ => Some(std::sync::Arc::new(LlmDistiller { cfg: cfg.clone() })),
    }
}

/// Build the BACKGROUND distiller per the auto mode, or `None` when auto is off
/// (or curation is off entirely). `Local` pins the transport to Ollama-only so
/// the background sweep can never fall back to the paid API, whatever the
/// explicit-call backend is.
pub fn make_auto_distiller(cfg: &CurateConfig) -> Option<std::sync::Arc<dyn Distiller>> {
    if cfg.backend == CurateBackend::Off {
        return None;
    }
    match cfg.auto {
        AutoDistill::Off => None,
        AutoDistill::On => make_distiller(cfg),
        AutoDistill::Local => Some(std::sync::Arc::new(LlmDistiller {
            cfg: CurateConfig { backend: CurateBackend::Ollama, ..cfg.clone() },
        })),
    }
}

/// The tiered production distiller (Ollama / Anthropic per [`CurateConfig`]).
pub struct LlmDistiller {
    cfg: CurateConfig,
}

#[async_trait]
impl Distiller for LlmDistiller {
    async fn distill_page(&self, page: &Page) -> Result<Distillation> {
        let prompt = build_prompt(page, MAX_BODY_CHARS);
        match self.cfg.backend {
            CurateBackend::Off => Err(anyhow!(
                "curation disabled (OCCIPITAL_CURATE_BACKEND=off)"
            )),
            CurateBackend::Ollama => self.distill_ollama(&prompt).await,
            CurateBackend::Anthropic => self.distill_anthropic(&prompt).await,
            CurateBackend::Auto => match self.distill_ollama(&prompt).await {
                Ok(d) => Ok(d),
                Err(ollama_err) => {
                    if self.cfg.anthropic_key.is_some() {
                        self.distill_anthropic(&prompt).await.map_err(|api_err| {
                            anyhow!("curate auto: ollama failed ({ollama_err}); anthropic failed ({api_err})")
                        })
                    } else {
                        Err(anyhow!(
                            "curate auto: ollama unreachable ({ollama_err}) and no ANTHROPIC_API_KEY \
                             for fallback. Point OCCIPITAL_CURATE_URL at a reachable Ollama (local \
                             or LAN), or set ANTHROPIC_API_KEY."
                        ))
                    }
                }
            },
        }
    }
}

impl LlmDistiller {
    /// Tier (a)/(b): an Ollama text model over `/api/generate`, JSON-forced.
    async fn distill_ollama(&self, prompt: &str) -> Result<Distillation> {
        // Short connect timeout so `auto` fails fast to the Anthropic fallback
        // when no Ollama is running; generous overall timeout for slow nodes.
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(120))
            .build()?;
        let url = format!("{}/api/generate", self.cfg.ollama_url);
        let body = json!({
            "model": self.cfg.ollama_model,
            "prompt": prompt,
            "format": "json",      // Ollama constrains the output to valid JSON
            "stream": false,
        });
        let resp = client
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        let status = resp.status();
        let data: serde_json::Value = resp.json().await.context("ollama response not JSON")?;
        if !status.is_success() {
            let msg = data["error"].as_str().unwrap_or("unknown error");
            return Err(anyhow!("ollama {status}: {msg}"));
        }
        let text = data["response"]
            .as_str()
            .ok_or_else(|| anyhow!("ollama: no 'response' field in {data}"))?;
        let parsed = parse_distillation(text)?;
        Ok(parsed.into_distillation(self.cfg.ollama_model.clone(), "ollama"))
    }

    /// Tier (c): the Anthropic Messages API.
    async fn distill_anthropic(&self, prompt: &str) -> Result<Distillation> {
        let key = self
            .cfg
            .anthropic_key
            .as_deref()
            .ok_or_else(|| anyhow!("anthropic curate: ANTHROPIC_API_KEY not set"))?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()?;
        let body = json!({
            "model": self.cfg.anthropic_model,
            "max_tokens": 1024,
            "messages": [{ "role": "user", "content": prompt }],
        });
        let resp = client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("POST anthropic messages")?;
        let status = resp.status();
        let data: serde_json::Value = resp.json().await.context("anthropic response not JSON")?;
        if !status.is_success() {
            let msg = data["error"]["message"].as_str().unwrap_or("unknown error");
            return Err(anyhow!("anthropic {status}: {msg}"));
        }
        let text = data["content"][0]["text"]
            .as_str()
            .ok_or_else(|| anyhow!("anthropic: unexpected response: {data}"))?;
        let parsed = parse_distillation(text)?;
        Ok(parsed.into_distillation(self.cfg.anthropic_model.clone(), "anthropic"))
    }
}

/// The distillation instruction: strict-JSON output over a capped reader-mode
/// body. Pure, so the shape is unit-testable.
pub fn build_prompt(page: &Page, max_body_chars: usize) -> String {
    let title = page.title.as_deref().unwrap_or("(untitled)");
    let body = truncate_chars(&page.markdown, max_body_chars);
    format!(
        "You are a knowledge curator. Distill the following web page into strict JSON.\n\
         Return ONLY one JSON object, no prose, no code fences, of exactly this shape:\n\
         {{\"summary\": \"...\", \"key_points\": [\"...\"], \"entities\": [\"...\"], \"tags\": [\"...\"]}}\n\
         - summary: 2-4 sentences stating what the page says (its facts, not a meta-description).\n\
         - key_points: 3-7 short, concrete claims or facts the page asserts.\n\
         - entities: proper nouns worth indexing (people, orgs, products, standards). May be empty.\n\
         - tags: 3-6 lowercase topic tags.\n\n\
         PAGE title: {title}\n\
         PAGE url: {url}\n\
         PAGE body (reader-mode markdown):\n{body}",
        url = page.url,
    )
}

/// The fields a model must return (pre-provenance). Deserialized leniently:
/// missing lists default to empty; only the summary is mandatory.
#[derive(Debug, Deserialize)]
pub struct ParsedDistillation {
    summary: String,
    #[serde(default)]
    key_points: Vec<String>,
    #[serde(default)]
    entities: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
}

impl ParsedDistillation {
    fn into_distillation(self, model: String, backend: &'static str) -> Distillation {
        Distillation {
            summary: truncate_chars(self.summary.trim(), MAX_SUMMARY_CHARS).to_string(),
            key_points: clean_list(self.key_points, MAX_KEY_POINTS, false),
            entities: clean_list(self.entities, MAX_ENTITIES, false),
            tags: clean_list(self.tags, MAX_TAGS, true),
            model,
            backend,
        }
    }
}

/// Trim/dedup/cap a model-produced list; `lowercase` normalizes tags.
fn clean_list(items: Vec<String>, cap: usize, lowercase: bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in items {
        let mut s = item.trim().to_string();
        if lowercase {
            s = s.to_lowercase();
        }
        if s.is_empty() || out.contains(&s) {
            continue;
        }
        out.push(s);
        if out.len() == cap {
            break;
        }
    }
    out
}

/// Extract + parse the JSON object from a model response. Tolerates prose or
/// code fences around the object (finds the outermost `{…}`); rejects output
/// with no parseable object or an empty summary. Pure.
pub fn parse_distillation(text: &str) -> Result<ParsedDistillation> {
    let start = text
        .find('{')
        .ok_or_else(|| anyhow!("no JSON object in model output"))?;
    let end = text
        .rfind('}')
        .ok_or_else(|| anyhow!("no JSON object in model output"))?;
    if end < start {
        return Err(anyhow!("malformed JSON object in model output"));
    }
    let parsed: ParsedDistillation = serde_json::from_str(&text[start..=end])
        .map_err(|e| anyhow!("model output is not a valid distillation: {e}"))?;
    if parsed.summary.trim().is_empty() {
        return Err(anyhow!("model returned an empty summary"));
    }
    Ok(parsed)
}

fn truncate_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> Page {
        Page {
            url: "https://e.test/rust".into(),
            title: Some("Rust 1.99 released".into()),
            byline: None,
            markdown: "Rust 1.99 ships const generics improvements and faster builds.".into(),
            links: vec![],
            forms: vec![],
            salvaged: false,
            js_required: false,
            content_hash: "h1".into(),
            markdown_alternate: None,
        }
    }

    #[test]
    fn parses_backend_aliases() {
        assert_eq!(CurateBackend::parse("ollama"), CurateBackend::Ollama);
        assert_eq!(CurateBackend::parse("LAN"), CurateBackend::Ollama);
        assert_eq!(CurateBackend::parse("anthropic"), CurateBackend::Anthropic);
        assert_eq!(CurateBackend::parse("api"), CurateBackend::Anthropic);
        assert_eq!(CurateBackend::parse("off"), CurateBackend::Off);
        assert_eq!(CurateBackend::parse("disabled"), CurateBackend::Off);
        assert_eq!(CurateBackend::parse("whatever"), CurateBackend::Auto);
        assert_eq!(CurateBackend::parse(""), CurateBackend::Auto);
    }

    #[test]
    fn off_backend_makes_no_distiller() {
        let cfg = CurateConfig { backend: CurateBackend::Off, ..Default::default() };
        assert!(make_distiller(&cfg).is_none());
        assert!(make_distiller(&CurateConfig::default()).is_some(), "auto builds one");
    }

    #[test]
    fn parses_auto_distill_aliases() {
        assert_eq!(AutoDistill::parse("local"), AutoDistill::Local);
        assert_eq!(AutoDistill::parse("OLLAMA"), AutoDistill::Local);
        assert_eq!(AutoDistill::parse("on"), AutoDistill::On);
        assert_eq!(AutoDistill::parse("1"), AutoDistill::On);
        assert_eq!(AutoDistill::parse("off"), AutoDistill::Off);
        assert_eq!(AutoDistill::parse(""), AutoDistill::Off);
        assert_eq!(AutoDistill::parse("whatever"), AutoDistill::Off, "unknown = off (safe)");
    }

    #[test]
    fn auto_distiller_matrix() {
        // Default: auto off → no background distiller even with curation on.
        assert!(make_auto_distiller(&CurateConfig::default()).is_none());
        // Auto on → background distiller exists.
        let on = CurateConfig { auto: AutoDistill::On, ..Default::default() };
        assert!(make_auto_distiller(&on).is_some());
        // Local: exists even when the explicit backend is Anthropic (pinned to
        // ollama internally — never the API).
        let local = CurateConfig {
            auto: AutoDistill::Local,
            backend: CurateBackend::Anthropic,
            ..Default::default()
        };
        assert!(make_auto_distiller(&local).is_some());
        // Curation off entirely → auto is off no matter the mode.
        let off = CurateConfig {
            auto: AutoDistill::On,
            backend: CurateBackend::Off,
            ..Default::default()
        };
        assert!(make_auto_distiller(&off).is_none());
    }

    #[test]
    fn prompt_carries_title_url_and_capped_body() {
        let mut p = page();
        p.markdown = "x".repeat(10_000);
        let prompt = build_prompt(&p, 100);
        assert!(prompt.contains("Rust 1.99 released"));
        assert!(prompt.contains("https://e.test/rust"));
        assert!(prompt.len() < 1_200, "body capped: {}", prompt.len());
        assert!(prompt.contains("\"summary\""), "states the output shape");
    }

    #[test]
    fn parses_clean_json() {
        let d = parse_distillation(
            r#"{"summary":"Rust 1.99 is out.","key_points":["const generics"],"entities":["Rust"],"tags":["rust","release"]}"#,
        )
        .unwrap()
        .into_distillation("m".into(), "ollama");
        assert_eq!(d.summary, "Rust 1.99 is out.");
        assert_eq!(d.key_points, vec!["const generics"]);
        assert_eq!(d.tags, vec!["rust", "release"]);
        assert_eq!(d.backend, "ollama");
    }

    #[test]
    fn parses_fenced_and_prose_wrapped_json() {
        let fenced = "```json\n{\"summary\": \"S.\", \"tags\": [\"t\"]}\n```";
        assert_eq!(parse_distillation(fenced).unwrap().summary, "S.");
        let prose = "Here is the distillation:\n{\"summary\": \"S2.\"}\nHope that helps!";
        assert_eq!(parse_distillation(prose).unwrap().summary, "S2.");
    }

    #[test]
    fn missing_lists_default_empty_but_summary_is_mandatory() {
        let d = parse_distillation(r#"{"summary":"Just a summary."}"#)
            .unwrap()
            .into_distillation("m".into(), "anthropic");
        assert!(d.key_points.is_empty() && d.entities.is_empty() && d.tags.is_empty());
        assert!(parse_distillation(r#"{"summary":""}"#).is_err(), "empty summary rejected");
        assert!(parse_distillation("no json here at all").is_err());
        assert!(parse_distillation(r#"{"tags":["x"]}"#).is_err(), "summary required");
    }

    #[test]
    fn lists_are_trimmed_deduped_capped_and_tags_lowercased() {
        let raw = format!(
            r#"{{"summary":"S.","tags":["Rust"," rust ","RELEASE","", {}]}}"#,
            (0..20).map(|i| format!("\"t{i}\"")).collect::<Vec<_>>().join(","),
        );
        let d = parse_distillation(&raw).unwrap().into_distillation("m".into(), "ollama");
        assert_eq!(d.tags[0], "rust");
        assert_eq!(d.tags[1], "release", "dupes+empties dropped, lowercased");
        assert_eq!(d.tags.len(), MAX_TAGS, "capped");
    }

    #[tokio::test]
    async fn off_distiller_errors_honestly() {
        let d = LlmDistiller { cfg: CurateConfig { backend: CurateBackend::Off, ..Default::default() } };
        let err = d.distill_page(&page()).await.unwrap_err().to_string();
        assert!(err.contains("disabled"), "got: {err}");
    }

    #[tokio::test]
    async fn anthropic_without_key_errors() {
        let d = LlmDistiller {
            cfg: CurateConfig { backend: CurateBackend::Anthropic, ..Default::default() },
        };
        let err = d.distill_page(&page()).await.unwrap_err().to_string();
        assert!(err.contains("ANTHROPIC_API_KEY"), "got: {err}");
    }
}
