use std::sync::Arc;

use anyhow::Result;
use occipital::{Config, Engine};
use tracing::info;

mod dispatch;
mod tools;
mod transport;

use transport::StdioTransport;

/// occipital-mcp — MCP-over-stdio server exposing the Occipital web tool surface
/// (`web_search` / `web_fetch` / `web_recall` / `web_save` / `web_forget`).
///
/// `web_search` + `web_fetch` are live (Phase 3); `web_recall` / `web_save` /
/// `web_forget` return an honest not-implemented error until the cache phases.
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr) // stdout is reserved for JSON-RPC; logs → stderr
        .init();

    let config = Config::from_env()?;
    let engine = Arc::new(Engine::from_config(&config)?);
    info!(
        tier = ?config.tier(),
        provider = %engine.provider_name(),
        "occipital-mcp starting"
    );

    let mut transport = StdioTransport::new();

    // MCP initialize handshake — guard on the method so a non-initialize first
    // message gets a proper method_not_found, not an init response.
    let init_req = transport.read().await?;
    let init_resp = if init_req["method"].as_str() == Some("initialize") {
        dispatch::handle_initialize(&init_req)
    } else {
        tracing::warn!("first message was not 'initialize': {:?}", init_req["method"]);
        dispatch::method_not_found(&init_req)
    };
    transport.write(&init_resp).await?;

    loop {
        match transport.read().await {
            Err(e) => {
                if e.to_string().contains("EOF") {
                    break; // client disconnected cleanly
                }
                tracing::error!("transport error: {e}");
                break;
            }
            Ok(msg) => {
                // Notifications carry no "id" — never respond to them.
                let is_notification = msg["id"].is_null()
                    || msg["method"].as_str().map(|m| m.starts_with("notifications/")).unwrap_or(false);
                if is_notification {
                    continue;
                }

                let method = msg["method"].as_str().unwrap_or("").to_string();
                let resp = match method.as_str() {
                    "tools/list" => dispatch::tools_list(&msg),
                    "tools/call" => dispatch::dispatch_tool(msg, Arc::clone(&engine)).await,
                    _ => dispatch::method_not_found(&msg),
                };
                transport.write(&resp).await?;
            }
        }
    }

    info!("occipital-mcp exiting");
    Ok(())
}
