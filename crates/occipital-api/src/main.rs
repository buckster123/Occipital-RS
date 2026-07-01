//! occipital-api — REST surface over the Engine: query (search/fetch/recall),
//! cache management (save/forget/gc/stats), and provider-key CRUD. The
//! management counterpart to the MCP tool surface.

use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
    Json, Router,
};
use occipital::{Config, Engine, Keys};
use serde::Deserialize;
use serde_json::json;

#[derive(Clone)]
struct AppState {
    engine:    Arc<Engine>,
    keys_file: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config = Config::from_env()?;
    let keys_file = config.keys_file.clone();
    let addr = std::env::var("OCCIPITAL_API_ADDR").unwrap_or_else(|_| "127.0.0.1:8799".to_string());
    let engine = Arc::new(Engine::from_config(&config)?);
    let state = AppState { engine, keys_file };

    let app = Router::new()
        .route("/health", get(health))
        .route("/stats", get(stats))
        .route("/search", get(search))
        .route("/fetch", get(fetch))
        .route("/recall", get(recall))
        .route("/distill", post(distill))
        .route("/save", post(save))
        .route("/forget", delete(forget))
        .route("/gc", post(gc))
        .route("/keys", get(keys_list))
        .route("/keys/:provider", put(keys_set).delete(keys_rm))
        .with_state(state);

    tracing::info!(%addr, "occipital-api starting");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

// ---- error plumbing ------------------------------------------------------

/// Maps an `anyhow::Error` into a 500 JSON body.
struct ApiError(anyhow::Error);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": self.0.to_string() }))).into_response()
    }
}
impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        ApiError(e.into())
    }
}
type ApiResult = Result<Json<serde_json::Value>, ApiError>;

// ---- handlers ------------------------------------------------------------

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok", "service": "occipital-api", "version": occipital::version() }))
}

async fn stats(State(s): State<AppState>) -> Json<serde_json::Value> {
    Json(json!({
        "provider":  s.engine.provider_name(),
        "semantic":  s.engine.semantic(),
        "curation":  s.engine.curation(),
        "cache":     s.engine.stats(),
    }))
}

#[derive(Deserialize)]
struct SearchQ {
    q:     String,
    limit: Option<usize>,
    #[serde(default)]
    fresh: bool,
}

async fn search(State(s): State<AppState>, Query(q): Query<SearchQ>) -> ApiResult {
    let (results, from_cache) = s.engine.search(&q.q, q.limit, q.fresh).await?;
    Ok(Json(json!({
        "kind": "results", "query": q.q, "provider": s.engine.provider_name(),
        "count": results.len(), "from_cache": from_cache, "results": results,
    })))
}

#[derive(Deserialize)]
struct FetchQ {
    url:   String,
    #[serde(default)]
    fresh: bool,
}

async fn fetch(State(s): State<AppState>, Query(q): Query<FetchQ>) -> ApiResult {
    let (page, from_cache) = s.engine.fetch(&q.url, q.fresh).await?;
    Ok(Json(json!({
        "kind": "page", "url": page.url, "title": page.title, "markdown": page.markdown,
        "links": page.links, "content_hash": page.content_hash, "from_cache": from_cache,
    })))
}

#[derive(Deserialize)]
struct RecallQ {
    q:     String,
    limit: Option<usize>,
}

async fn recall(State(s): State<AppState>, Query(q): Query<RecallQ>) -> ApiResult {
    let hits = s.engine.recall(&q.q, q.limit).await?;
    Ok(Json(json!({ "kind": "recall", "query": q.q, "count": hits.len(), "hits": hits })))
}

#[derive(Deserialize, Default)]
struct DistillBody {
    url:   Option<String>,
    limit: Option<usize>,
}

/// `POST /distill` — body optional: `{url}` distills one page, `{limit}` bounds
/// a sweep of not-yet-distilled pages, `{}` sweeps the default batch.
async fn distill(State(s): State<AppState>, body: Option<Json<DistillBody>>) -> ApiResult {
    let Json(b) = body.unwrap_or_default();
    let report = s.engine.distill(b.url.as_deref(), b.limit).await?;
    Ok(Json(json!({
        "kind": "distill", "count": report.distilled.len(),
        "distilled": report.distilled, "failed": report.failed, "remaining": report.remaining,
    })))
}

#[derive(Deserialize)]
struct UrlQ {
    url: String,
}

async fn save(State(s): State<AppState>, Query(q): Query<UrlQ>) -> ApiResult {
    let page = s.engine.save(&q.url).await?;
    Ok(Json(json!({ "status": "saved", "pinned": true, "url": page.url, "title": page.title })))
}

async fn forget(State(s): State<AppState>, Query(q): Query<UrlQ>) -> ApiResult {
    let removed = s.engine.forget(&q.url)?;
    Ok(Json(json!({ "status": "ok", "url": q.url, "removed": removed })))
}

async fn gc(State(s): State<AppState>) -> ApiResult {
    let pruned = s.engine.gc()?;
    Ok(Json(json!({ "status": "ok", "pruned": pruned })))
}

async fn keys_list(State(s): State<AppState>) -> Json<serde_json::Value> {
    let listed = Keys::load(&s.keys_file).list();
    Json(json!({ "keys": listed.into_iter().map(|(p, r)| json!({ "provider": p, "redacted": r })).collect::<Vec<_>>() }))
}

#[derive(Deserialize)]
struct KeyBody {
    key: String,
}

async fn keys_set(State(s): State<AppState>, Path(provider): Path<String>, Json(body): Json<KeyBody>) -> ApiResult {
    let mut keys = Keys::load(&s.keys_file);
    keys.set(&provider, &body.key);
    keys.save()?;
    Ok(Json(json!({ "status": "ok", "provider": provider })))
}

async fn keys_rm(State(s): State<AppState>, Path(provider): Path<String>) -> ApiResult {
    let mut keys = Keys::load(&s.keys_file);
    let removed = keys.remove(&provider);
    if removed {
        keys.save()?;
    }
    Ok(Json(json!({ "status": "ok", "provider": provider, "removed": removed })))
}
