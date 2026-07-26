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

/// Wire cap for a page's link list. One exploratory `web_dom` on a portal
/// page returned 570 links of mostly interlanguage chrome (apex1 field
/// report, 2026-07-26) — more context than the page is worth. The cap is a
/// VIEW: the full list stays in the page cache, ordinals are positions in the
/// full list (so the entries that survive keep their meaning), and
/// `links_total` reports the true count so truncation is visible. `web_dom`
/// pages through the rest via `links_from`/`limit`.
const LINK_CAP: usize = 120;

fn capped_links<T: serde::Serialize>(links: &[T]) -> (Value, usize) {
    let total = links.len();
    (json!(links[..total.min(LINK_CAP)]), total)
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
            let (links, links_total) = capped_links(&page.links);
            let mut out = json!({
                "kind":         "page",
                "url":          page.url,
                "title":        page.title,
                "markdown":     page.markdown,
                "links":        links,
                "links_total":  links_total,
                "forms":        page.forms,
                "salvaged":     page.salvaged,
                "js_required":  page.js_required,
                "content_hash": page.content_hash,
                "from_cache":   from_cache,
            });
            // The page's own offer of authored markdown — reported, never
            // auto-followed (fetch it like any URL if it beats reader-mode).
            if let Some(alt) = &page.markdown_alternate {
                out["markdown_alternate"] = json!(alt);
            }
            // Verbatim-passthrough provenance (text/markdown | text/plain).
            if let Some(sf) = &page.source_format {
                out["source_format"] = json!(sf);
            }
            Ok(out)
        }
        "web_dom" => {
            let url = args["url"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("url (non-empty string) required"))?;
            let fresh = args["fresh"].as_bool().unwrap_or(false);
            let view = engine.dom(url, fresh).await?;
            // The registry window: ordinals (`idx`) are stable against the
            // FULL list — the window is a view, never a re-index.
            let from = args["links_from"].as_u64().unwrap_or(1).max(1) as usize;
            let limit = args["limit"].as_u64().unwrap_or(LINK_CAP as u64) as usize;
            let links_total = view.links.len();
            let window: Vec<_> = view.links.iter().skip(from - 1).take(limit).collect();
            let mut out = json!({
                "kind":         "dom",
                "url":          view.url,
                "title":        view.title,
                "links":        window,
                "links_total":  links_total,
                "forms":        view.forms,
                "content_hash": view.content_hash,
                "from_cache":   view.from_cache,
                "snapshot":     view.snapshot,
                "salvaged":     view.salvaged,
                "js_required":  view.js_required,
            });
            if url.starts_with("result:") {
                out["handle"] = json!(url);
            }
            Ok(out)
        }
        "web_click" => {
            let url = args["url"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("url (non-empty string) required"))?;
            let element = args["element"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("element (link:N or form:N) required"))?;
            let r = engine.click(url, element).await?;
            let (links, links_total) = capped_links(&r.page.links);
            let mut out = json!({
                "kind":         "click",
                "element":      r.element,
                "source_url":   r.source_url,
                "target_url":   r.target_url,
                "url":          r.page.url,
                "title":        r.page.title,
                "markdown":     r.page.markdown,
                "links":        links,
                "links_total":  links_total,
                "forms":        r.page.forms,
                "salvaged":     r.page.salvaged,
                "js_required":  r.page.js_required,
                "content_hash": r.page.content_hash,
                "from_cache":   r.from_cache,
                "status":       r.status,
            });
            if let Some(alt) = &r.page.markdown_alternate {
                out["markdown_alternate"] = json!(alt);
            }
            if let Some(sf) = &r.page.source_format {
                out["source_format"] = json!(sf);
            }
            // A POST result is not addressable by URL — surface its working-
            // memory handle so the next verb can act on THIS page.
            if let Some(h) = &r.handle {
                out["handle"] = json!(h);
            }
            // Echo which hop this came from: source_url alone can't tell
            // (every hop of a POST walk shares the same final_url), so a
            // multi-hop transcript would read as three identical clicks
            // (apex1 verification round, 2026-07-26).
            if url.starts_with("result:") {
                out["from_handle"] = json!(url);
            }
            Ok(out)
        }
        "web_submit" => {
            let url = args["url"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("url (non-empty string) required"))?;
            let form = args["form"]
                .as_u64()
                .filter(|&n| n > 0)
                .ok_or_else(|| anyhow::anyhow!("form (1-based ordinal) required"))?
                as usize;
            let fields: Vec<(String, String)> = args["fields"]
                .as_object()
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| {
                            let val = v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
                            (k.clone(), val)
                        })
                        .collect()
                })
                .unwrap_or_default();
            let r = engine.submit(url, form, &fields).await?;
            let (links, links_total) = capped_links(&r.page.links);
            let mut out = json!({
                "kind":         "submit",
                "source_url":   r.source_url,
                "form":         r.form,
                "action":       r.action,
                "method":       r.method,
                "sent":         r.sent,
                "status":       r.status,
                "url":          r.page.url,
                "title":        r.page.title,
                "markdown":     r.page.markdown,
                "links":        links,
                "links_total":  links_total,
                "forms":        r.page.forms,
                "salvaged":     r.page.salvaged,
                "js_required":  r.page.js_required,
                "content_hash": r.page.content_hash,
                "cached":       r.cached,
            });
            if let Some(alt) = &r.page.markdown_alternate {
                out["markdown_alternate"] = json!(alt);
            }
            if let Some(sf) = &r.page.source_format {
                out["source_format"] = json!(sf);
            }
            // A POST result is not addressable by URL — surface its working-
            // memory handle so pagination is discoverable without docs.
            if let Some(h) = &r.handle {
                out["handle"] = json!(h);
            }
            // Which hop was this? source_url is identical across a POST walk
            // (same final_url every time) — echo the input handle so the
            // transcript self-documents the chain.
            if url.starts_with("result:") {
                out["from_handle"] = json!(url);
            }
            Ok(out)
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
        async fn request(&self, req: occipital::fetch::HttpRequest) -> anyhow::Result<FetchResponse> {
            self.get(&req.url).await
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
        for expected in ["web_search", "web_fetch", "web_dom", "web_click", "web_submit",
                         "web_recall", "web_save", "web_forget"] {
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
    async fn web_dom_returns_the_element_registry() {
        let html = r#"<html><head><title>S</title></head><body><main>
            <p><a href="/next">next page</a></p>
            <form action="/search"><input type="search" name="q"><button>Go</button></form>
            </main></body></html>"#;
        let msg = json!({"jsonrpc":"2.0","id":9,"method":"tools/call",
            "params":{"name":"web_dom","arguments":{"url":"https://example.com/p"}}});
        let resp = dispatch_tool(msg, engine_with(html)).await;
        let out: Value = serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(out["kind"], "dom");
        assert_eq!(out["links"][0]["idx"], 1);
        assert_eq!(out["links"][0]["url"], "https://example.com/next");
        assert_eq!(out["forms"][0]["idx"], 1);
        assert_eq!(out["forms"][0]["action"], "https://example.com/search");
        assert_eq!(out["forms"][0]["fields"][0]["name"], "q");
        assert_eq!(out["snapshot"], true, "the read-through fetch left a snapshot");
    }

    const CLICKABLE: &str = r#"<html><head><title>C</title></head><body><main>
        <p><a href="/next">next page</a></p>
        <form action="/search"><input type="text" name="q"><button>Go</button></form>
        </main></body></html>"#;

    #[tokio::test]
    async fn web_click_follows_a_link_by_ordinal() {
        let msg = json!({"jsonrpc":"2.0","id":10,"method":"tools/call",
            "params":{"name":"web_click","arguments":{"url":"https://example.com/p","element":"link:1"}}});
        let resp = dispatch_tool(msg, engine_with(CLICKABLE)).await;
        let out: Value = serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(out["kind"], "click");
        assert_eq!(out["target_url"], "https://example.com/next");
        assert_eq!(out["source_url"], "https://example.com/p");
        assert!(out["markdown"].as_str().is_some());
    }

    #[tokio::test]
    async fn web_submit_fills_and_submits_a_get_form() {
        let msg = json!({"jsonrpc":"2.0","id":11,"method":"tools/call",
            "params":{"name":"web_submit","arguments":{
                "url":"https://example.com/p","form":1,"fields":{"q":"rust"}}}});
        let resp = dispatch_tool(msg, engine_with(CLICKABLE)).await;
        let out: Value = serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(out["kind"], "submit");
        assert_eq!(out["method"], "get");
        assert_eq!(out["url"], "https://example.com/search?q=rust");
        assert_eq!(out["sent"][0]["name"], "q");
        assert_eq!(out["sent"][0]["value"], "rust");
    }

    #[tokio::test]
    async fn web_click_requires_a_valid_element() {
        let msg = json!({"jsonrpc":"2.0","id":12,"method":"tools/call",
            "params":{"name":"web_click","arguments":{"url":"https://example.com/p"}}});
        let resp = dispatch_tool(msg, engine_with(CLICKABLE)).await;
        assert!(resp["error"]["message"].as_str().unwrap().contains("element"));
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

    fn portal_html(n: usize) -> String {
        let links: String = (1..=n)
            .map(|i| format!(r#"<a href="/p{i}">link {i}</a> "#))
            .collect();
        format!("<html><head><title>Portal</title></head><body><main><p>{links}</p></main></body></html>")
    }

    #[tokio::test]
    async fn dom_link_window_caps_with_stable_full_list_ordinals() {
        let html = portal_html(150);

        // Default window: first LINK_CAP links, true total reported.
        let msg = json!({"jsonrpc":"2.0","id":10,"method":"tools/call",
            "params":{"name":"web_dom","arguments":{"url":"https://portal.test/"}}});
        let resp = dispatch_tool(msg, engine_with(&html)).await;
        let out: Value = serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(out["links_total"], 150, "truncation must be visible");
        assert_eq!(out["links"].as_array().unwrap().len(), LINK_CAP);
        assert_eq!(out["links"][0]["idx"], 1);

        // A window into the tail: ordinals are FULL-list positions — the
        // window is a view, never a re-index (else link:N silently changes
        // meaning between calls).
        let msg = json!({"jsonrpc":"2.0","id":11,"method":"tools/call",
            "params":{"name":"web_dom","arguments":{"url":"https://portal.test/","links_from":121,"limit":10}}});
        let resp = dispatch_tool(msg, engine_with(&html)).await;
        let out: Value = serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(out["links"].as_array().unwrap().len(), 10);
        assert_eq!(out["links"][0]["idx"], 121);
        assert_eq!(out["links"][0]["url"], "https://portal.test/p121");
        assert_eq!(out["links_total"], 150);
    }

    #[tokio::test]
    async fn handle_hops_echo_from_handle_for_the_audit_trail() {
        // Every hop of a POST walk shares the same final_url, so source_url
        // alone reads as three identical submits — the echoed input handle is
        // what makes the chain self-documenting.
        let html = r#"<html><body><main>
            <form action="/page" method="post">
              <input type="hidden" name="page" value="2"><button>Next</button>
            </form></main></body></html>"#;
        let msg = json!({"jsonrpc":"2.0","id":20,"method":"tools/call",
            "params":{"name":"web_submit","arguments":{"url":"https://walk.test/","form":1}}});
        let resp = dispatch_tool(msg, engine_with(html)).await;
        let out: Value = serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert!(out["handle"].as_str().is_some(), "POST result mints a handle");
        assert!(out.get("from_handle").is_none(), "hop 1 came from a URL, not a handle");

        // Hop 2 THROUGH the handle: the same engine must be reused (the
        // result store is in-memory), and the input handle must be echoed.
        let engine = engine_with(html);
        let msg = json!({"jsonrpc":"2.0","id":21,"method":"tools/call",
            "params":{"name":"web_submit","arguments":{"url":"https://walk.test/","form":1}}});
        let resp = dispatch_tool(msg, Arc::clone(&engine)).await;
        let out: Value = serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        let handle = out["handle"].as_str().unwrap().to_string();
        let msg = json!({"jsonrpc":"2.0","id":22,"method":"tools/call",
            "params":{"name":"web_submit","arguments":{"url":handle,"form":1}}});
        let resp = dispatch_tool(msg, engine).await;
        let out: Value = serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(out["from_handle"].as_str(), Some(handle.as_str()), "the hop chain is auditable");
        assert!(out["handle"].as_str().is_some(), "…and the walk can continue");
    }

    #[tokio::test]
    async fn web_fetch_caps_links_and_reports_the_total() {
        let html = portal_html(150);
        let msg = json!({"jsonrpc":"2.0","id":12,"method":"tools/call",
            "params":{"name":"web_fetch","arguments":{"url":"https://portal.test/"}}});
        let resp = dispatch_tool(msg, engine_with(&html)).await;
        let out: Value = serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(out["links_total"], 150);
        assert_eq!(out["links"].as_array().unwrap().len(), LINK_CAP,
            "one portal page must not dump 570 links into the context");
    }
}
