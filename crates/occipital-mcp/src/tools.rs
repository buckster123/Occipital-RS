//! The advertised tool surface (input schemas). The handlers land per roadmap
//! phase; the schemas are stable from Phase 0 so any MCP client can introspect
//! the contract now.

use serde_json::{json, Value};

/// Every tool Occipital advertises, in a stable order.
pub const TOOL_NAMES: &[&str] = &[
    "web_search",
    "web_fetch",
    "web_dom",
    "web_recall",
    "web_save",
    "web_forget",
    "web_distill",
];

pub fn all_tool_schemas() -> Vec<Value> {
    TOOL_NAMES.iter().map(|&name| tool_schema(name)).collect()
}

fn tool_schema(name: &str) -> Value {
    match name {
        "web_search" => json!({
            "name": "web_search",
            "description": "Search the web for a query. Checks the local read-through cache first; only a miss or stale entry hits live providers (politely, rate-limited). Returns ranked results as clean text.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query":    { "type": "string", "description": "The search query" },
                    "limit":    { "type": "integer", "description": "Max results (default: 5)" },
                    "provider": { "type": "string", "description": "Override the configured search provider (duckduckgo/searxng/brave/tavily/bing)" },
                    "fresh":    { "type": "boolean", "description": "Bypass the cache and force a live search (default: false)" }
                },
                "required": ["query"]
            }
        }),
        "web_fetch" => json!({
            "name": "web_fetch",
            "description": "Fetch a single URL and return it as clean reader-mode Markdown (no nav/ads/chrome) plus its links. Cache-first with a conditional refresh; a live fetch is polite and rate-limited.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url":   { "type": "string", "description": "The URL to fetch" },
                    "fresh": { "type": "boolean", "description": "Bypass the cache and force a live fetch (default: false)" }
                },
                "required": ["url"]
            }
        }),
        "web_dom" => json!({
            "name": "web_dom",
            "description": "The element registry of a page: its links and forms with stable ordinals (link idx, form idx, field names) — the handles the interaction verbs address. Cache-first like web_fetch; `snapshot` reports whether the raw page is still held for ordinal resolution. Use when you need the interactive surface rather than the prose.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url":   { "type": "string", "description": "The page whose element registry to return" },
                    "fresh": { "type": "boolean", "description": "Bypass the cache and force a live fetch (default: false)" }
                },
                "required": ["url"]
            }
        }),
        "web_recall" => json!({
            "name": "web_recall",
            "description": "Search ONLY pages already read and cached (no live web hit). Semantic when embeddings are on, keyword (FTS5) otherwise. Use to recall what was previously found.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "What to recall from cached pages" },
                    "limit": { "type": "integer", "description": "Max results (default: 5)" }
                },
                "required": ["query"]
            }
        }),
        "web_save" => json!({
            "name": "web_save",
            "description": "Force-cache (pin) a URL so it is fetched, stored, and exempt from decay-based eviction until its TTL. Use to hold a reference page.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "The URL to pin in the cache" }
                },
                "required": ["url"]
            }
        }),
        "web_distill" => json!({
            "name": "web_distill",
            "description": "Distill already-read pages into curated knowledge — a summary, key points, entities, and topic tags — via the configured LLM backend (local Ollama or API). With url: distill that page (fetching it first if uncached; free if already distilled and unchanged). Without url: sweep a few not-yet-distilled cached pages. Distilled pages sharpen web_recall: recall then returns the summary and tags instead of a raw-body snippet.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url":   { "type": "string", "description": "Distill this page (fetched first if not cached)" },
                    "limit": { "type": "integer", "description": "Sweep size when no url is given (default: 3, max: 10)" }
                },
                "required": []
            }
        }),
        "web_forget" => json!({
            "name": "web_forget",
            "description": "Evict a URL (or matching set) from the cache.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url":   { "type": "string", "description": "The URL to evict" },
                    "query": { "type": "string", "description": "Alternatively, evict cached pages matching this query" }
                },
                "required": []
            }
        }),
        // Should be unreachable: TOOL_NAMES is the source of truth.
        other => json!({ "name": other, "description": "(unknown)", "inputSchema": { "type": "object" } }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_named_tool_has_a_well_formed_schema() {
        for &name in TOOL_NAMES {
            let s = tool_schema(name);
            assert_eq!(s["name"], name);
            assert!(s["description"].as_str().map(|d| !d.is_empty()).unwrap_or(false),
                "{name} needs a description");
            assert_eq!(s["inputSchema"]["type"], "object", "{name} schema must be an object");
        }
    }

    #[test]
    fn all_tool_schemas_covers_every_name() {
        assert_eq!(all_tool_schemas().len(), TOOL_NAMES.len());
    }
}
