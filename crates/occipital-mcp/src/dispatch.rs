//! JSON-RPC dispatch — handshake, tools/list, and tools/call routing.
//!
//! Phase 0 routes every advertised tool to an honest not-implemented error (the
//! surface is real so clients can introspect it; the logic lands per roadmap
//! phase). The panic-isolation + error-wrapping shape mirrors Cerebro-MCP so a
//! handler fault can never take the daemon down.

use std::sync::Arc;

use occipital::Config;
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
pub async fn dispatch_tool(msg: Value, config: Arc<Config>) -> Value {
    let id = msg["id"].clone();

    let handle = tokio::spawn(async move {
        let params = &msg["params"];
        let name = params["name"].as_str().unwrap_or("").to_string();
        let args = params["arguments"].clone();
        route(&name, &args, config).await
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

/// The tool router. Phase 0: known tools return an honest not-implemented error
/// (NOT a success stub — that would read as "it worked"); unknown tools error too.
async fn route(name: &str, _args: &Value, _config: Arc<Config>) -> anyhow::Result<Value> {
    match name {
        "web_search" | "web_fetch" | "web_recall" | "web_save" | "web_forget" => {
            anyhow::bail!("tool not implemented yet (Phase 0 scaffold): {name}")
        }
        _ => anyhow::bail!("tool not found: {name}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Arc<Config> {
        Arc::new(Config::from_env().unwrap())
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
    async fn known_tool_returns_honest_not_implemented_not_a_success() {
        let msg = json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"web_search","arguments":{"query":"x"}}});
        let resp = dispatch_tool(msg, cfg()).await;
        assert!(resp["result"].is_null(), "must NOT report success for an unimplemented tool");
        assert!(resp["error"]["message"].as_str().unwrap().contains("not implemented"));
    }

    #[tokio::test]
    async fn unknown_tool_errors() {
        let msg = json!({"jsonrpc":"2.0","id":4,"method":"tools/call",
            "params":{"name":"definitely_not_a_tool","arguments":{}}});
        let resp = dispatch_tool(msg, cfg()).await;
        assert!(resp["error"]["message"].as_str().unwrap().contains("not found"));
    }
}
