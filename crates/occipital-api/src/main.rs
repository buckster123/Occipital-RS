//! occipital-api — REST surface for cache management + queries.
//!
//! Phase 0: a health endpoint and the server skeleton. The management/query
//! routes (search, fetch, recall, page CRUD, provider keys, gc, stats) land in
//! Phase 8 — see `docs/build-roadmap.md`.

use axum::{routing::get, Json, Router};
use occipital::Config;
use serde_json::json;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config = Config::from_env()?;
    let addr = std::env::var("OCCIPITAL_API_ADDR").unwrap_or_else(|_| "127.0.0.1:8799".to_string());

    let app = Router::new().route("/health", get(health));

    tracing::info!(%addr, tier = ?config.tier(), "occipital-api starting");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok", "service": "occipital-api", "version": occipital::version() }))
}
