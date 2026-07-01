//! JSON-RPC dispatch — handshake, tools/list, and tools/call routing.
//!
//! Phase 0 routes every advertised tool to an honest not-implemented error (the
//! surface is real so clients can introspect it; the logic lands per roadmap
//! phase). The panic-isolation + error-wrapping shape mirrors Cerebro-MCP so a
//! handler fault can never take the daemon down.

use std::sync::Arc;

use occipital::Engine;
use serde_json::{json, Value};

use crate::tools;

pub fn handle_initialize(req: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": req["id"],
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "occipital-mcp",
                "version": env!("CARGO_PKG_VERSION")
            }
        }
    })
}

pub fn method_not_found(req: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": req["id"],
        "error": { "code": -32601, "message": "method not found" }
    })
}

pub fn tools_list(req: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": req["id"],
        "result": { "tools": tools::all_tool_schemas() }
    })
}

/// Route a `tools/call`, isolating handler panics on a dedicated task so a fault
/// can never unwind into the main loop and take the daemon down.
pub async fn dispatch_tool(msg: Value, engine: Arc<Engine>) -> Value {
    let id = msg["id"].clone();

    let handle = tokio::spawn(async move {
        let params = &msg["params"];
        let name = params["name"].as_str().unwrap_or("").to_string();
        let args = params["arguments"].clone();
        route(&name, &args, engine).await
    });

    match handle.await {
        Ok(Ok(v)) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "content": [{ "type": "text", "text": v.to_string() }] }
        }),
        Ok(Err(e)) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": e.to_string() }
        }),
        Err(join_err) => {
            tracing::error!("tool handler panicked: {join_err}");
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32603, "message": "internal error: tool handler panicked" }
            })
        }
    }
}

/// The tool router. `web_search` + `web_fetch` are live; the cache-backed tools
/// (`web_recall` / `web_save` / `web_forget`) return an honest not-implemented
/// error (NOT a success stub) until the cache phases; unknown tools error too.
async fn route(name: &str, args: &Value, engine: Arc<Engine>) -> anyhow::Result<Value> {
    match name {
        "web_search" => {
            let query = args["query"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("query (non-empty string) required"))?;
            let limit = args["limit"].as_u64().map(|n| n as usize);
            let fresh = args["fresh"].as_bool().unwrap_or(false);
            let (results, from_cache) = engine.search(query, limit, fresh).await?;
            // Flat, `kind`-discriminated result: it IS both the agent payload and
            // the follow-along view (docs/follow-along.md).
            Ok(json!({
                "kind":       "results",
                "query":      query,
                "provider":   engine.provider_name(),
                "count":      results.len(),
                "from_cache": from_cache,
                "results":    results,
            }))
        }
        "web_fetch" => {
            let url = args["url"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("url (non-empty string) required"))?;
            let fresh = args["fresh"].as_bool().unwrap_or(false);
            let (page, from_cache) = engine.fetch(url, fresh).await?;
            Ok(json!({
                "kind":         "page",
                "url":          page.url,
                "title":        page.title,
                "markdown":     page.markdown,
                "links":        page.links,
                "content_hash": page.content_hash,
                "from_cache":   from_cache,
            }))
        }
        "web_save" => {
            let url = args["url"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("url (non-empty string) required"))?;
            let page = engine.save(url).await?;
            Ok(json!({
                "kind":         "page",
                "status":       "saved",
                "pinned":       true,
                "url":          page.url,
                "title":        page.title,
                "content_hash": page.content_hash,
            }))
        }
        "web_forget" => {
            let url = args["url"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("url (non-empty string) required — query-based forget lands with recall"))?;
            let removed = engine.forget(url)?;
            Ok(json!({ "status": "ok", "url": url, "removed": removed }))
        }
        "web_recall" => {
            let query = args["query"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("query (non-empty string) required"))?;
            let limit = args["limit"].as_u64().map(|n| n as usize);
            let hits = engine.recall(query, limit).await?;
            Ok(json!({
                "kind":  "recall",
                "query": query,
                "count": hits.len(),
                "hits":  hits,
            }))
        }
        "web_distill" => {
            let url = args["url"].as_str().filter(|s| !s.trim().is_empty());
            let limit = args["limit"].as_u64().map(|n| n as usize);
            let report = engine.distill(url, limit).await?;
            Ok(json!({
                "kind":      "distill",
                "count":     report.distilled.len(),
                "distilled": report.distilled,
                "failed":    report.failed,
                "remaining": report.remaining,
            }))
        }
        _ => anyhow::bail!("tool not found: {name}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use occipital::fetch::{FetchResponse, Fetcher, Source};
    use occipital::providers::DuckDuckGo;

    /// A fetcher returning a fixed body — drives the engine without a network.
    struct Canned(Vec<u8>);
    #[async_trait]
    impl Fetcher for Canned {
        async fn get(&self, url: &str) -> anyhow::Result<FetchResponse> {
            Ok(FetchResponse {
                final_url:     url.to_string(),
                status:        200,
                content_type:  None,
                etag:          None,
                last_modified: None,
                body:          self.0.clone(),
                source:        Source::Network,
            })
        }
    }

    fn engine_with(body: &str) -> Arc<Engine> {
        let fetcher = Arc::new(Canned(body.as_bytes().to_vec()));
        let cache = Arc::new(occipital::Cache::open_in_memory().unwrap());
        Arc::new(Engine::with_parts(fetcher, Box::new(DuckDuckGo), Some(cache), 5, 3600))
    }

    #[test]
    fn initialize_echoes_id_and_names_the_server() {
        let req = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}});
        let resp = handle_initialize(&req);
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["serverInfo"]["name"], "occipital-mcp");
        assert_eq!(resp["result"]["protocolVersion"], "2024-11-05");
    }

    #[test]
    fn tools_list_advertises_the_web_surface() {
        let resp = tools_list(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}));
        let names: Vec<String> = resp["result"]["tools"].as_array().unwrap().iter()
            .map(|t| t["name"].as_str().unwrap().to_string()).collect();
        for expected in ["web_search", "web_fetch", "web_recall", "web_save", "web_forget"] {
            assert!(names.contains(&expected.to_string()), "must advertise {expected}: {names:?}");
        }
    }

    #[tokio::test]
    async fn web_search_returns_ranked_results() {
        let ddg = r#"<div class="result"><div class="links_main">
            <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Frust-lang.org%2F">Rust</a>
            <a class="result__snippet">A safe language.</a></div></div>"#;
        let msg = json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"web_search","arguments":{"query":"rust"}}});
        let resp = dispatch_tool(msg, engine_with(ddg)).await;
        let out: Value = serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(out["kind"], "results");
        assert_eq!(out["count"], 1);
        assert_eq!(out["results"][0]["url"], "https://rust-lang.org/");
    }

    #[tokio::test]
    async fn web_fetch_returns_reader_mode() {
        let html = "<html><head><title>T</title></head><body><main><h1>Hi</h1><p>body</p></main></body></html>";
        let msg = json!({"jsonrpc":"2.0","id":4,"method":"tools/call",
            "params":{"name":"web_fetch","arguments":{"url":"https://example.com/p"}}});
        let resp = dispatch_tool(msg, engine_with(html)).await;
        let out: Value = serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(out["kind"], "page");
        assert_eq!(out["title"], "T");
        assert!(out["markdown"].as_str().unwrap().contains("# Hi"));
    }

    #[tokio::test]
    async fn web_search_requires_a_query() {
        let msg = json!({"jsonrpc":"2.0","id":5,"method":"tools/call",
            "params":{"name":"web_search","arguments":{}}});
        let resp = dispatch_tool(msg, engine_with("")).await;
        assert!(resp["result"].is_null());
        assert!(resp["error"]["message"].as_str().unwrap().contains("query"));
    }

    #[tokio::test]
    async fn web_recall_returns_a_recall_payload() {
        // Empty cache → zero hits, but a well-formed recall result (not an error).
        let msg = json!({"jsonrpc":"2.0","id":6,"method":"tools/call",
            "params":{"name":"web_recall","arguments":{"query":"rust"}}});
        let resp = dispatch_tool(msg, engine_with("")).await;
        assert!(resp["error"].is_null(), "recall should not error: {}", resp["error"]);
        let out: Value = serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(out["kind"], "recall");
        assert_eq!(out["count"], 0);
    }

    #[tokio::test]
    async fn web_distill_without_a_backend_errors_honestly() {
        // The test engine has no distiller (curation off) → an honest error,
        // exercising the route + arg plumbing.
        let msg = json!({"jsonrpc":"2.0","id":8,"method":"tools/call",
            "params":{"name":"web_distill","arguments":{}}});
        let resp = dispatch_tool(msg, engine_with("")).await;
        assert!(resp["error"]["message"].as_str().unwrap().contains("curation disabled"));
    }

    #[tokio::test]
    async fn unknown_tool_errors() {
        let msg = json!({"jsonrpc":"2.0","id":7,"method":"tools/call",
            "params":{"name":"definitely_not_a_tool","arguments":{}}});
        let resp = dispatch_tool(msg, engine_with("")).await;
        assert!(resp["error"]["message"].as_str().unwrap().contains("not found"));
    }
}
