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

    // Background auto-curation (OCCIPITAL_AUTO_DISTILL=local|on) — same sweep
    // as occipital-mcp, for API-only deployments.
    if engine.auto_curation() {
        let e = Arc::clone(&engine);
        let interval = config.curate.auto_interval_secs;
        tracing::info!(interval, "auto-distill sweep enabled");
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
                match e.auto_distill_tick().await {
                    Ok(Some(r)) => tracing::info!(
                        distilled = r.distilled.len(),
                        failed = r.failed.len(),
                        remaining = r.remaining,
                        "auto-distill sweep"
                    ),
                    Ok(None) => {}
                    Err(err) => tracing::warn!("auto-distill sweep failed: {err}"),
                }
            }
        });
    }

    let state = AppState { engine, keys_file };

    let app = Router::new()
        .route("/health", get(health))
        .route("/stats", get(stats))
        .route("/search", get(search))
        .route("/fetch", get(fetch))
        .route("/dom", get(dom))
        .route("/click", post(click))
        .route("/submit", post(submit))
        .route("/recall", get(recall))
        .route("/distill", post(distill))
        .route("/related", get(related))
        .route("/save", post(save))
        .route("/forget", delete(forget))
        .route("/log", get(request_log))
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
        "auto_curation": s.engine.auto_curation(),
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
    let mut out = json!({
        "kind": "page", "url": page.url, "title": page.title, "markdown": page.markdown,
        "links": page.links, "forms": page.forms, "salvaged": page.salvaged,
        "js_required": page.js_required, "content_hash": page.content_hash,
        "from_cache": from_cache,
    });
    if let Some(alt) = &page.markdown_alternate {
        out["markdown_alternate"] = json!(alt);
    }
    if let Some(sf) = &page.source_format {
        out["source_format"] = json!(sf);
    }
    Ok(Json(out))
}

/// `GET /dom` — the element registry (links + forms with stable ordinals).
async fn dom(State(s): State<AppState>, Query(q): Query<FetchQ>) -> ApiResult {
    let view = s.engine.dom(&q.url, q.fresh).await?;
    Ok(Json(json!({
        "kind": "dom", "url": view.url, "title": view.title, "links": view.links,
        "forms": view.forms, "content_hash": view.content_hash,
        "from_cache": view.from_cache, "snapshot": view.snapshot,
        "salvaged": view.salvaged, "js_required": view.js_required,
    })))
}

#[derive(Deserialize)]
struct ClickBody {
    url:     String,
    element: String,
}

/// `POST /click` — follow a link / submit a form by registry ordinal.
async fn click(State(s): State<AppState>, Json(b): Json<ClickBody>) -> ApiResult {
    let r = s.engine.click(&b.url, &b.element).await?;
    let mut out = json!({
        "kind": "click", "element": r.element, "source_url": r.source_url,
        "target_url": r.target_url, "url": r.page.url, "title": r.page.title,
        "markdown": r.page.markdown, "links": r.page.links, "forms": r.page.forms,
        "salvaged": r.page.salvaged, "js_required": r.page.js_required,
        "content_hash": r.page.content_hash, "from_cache": r.from_cache, "status": r.status,
    });
    if let Some(h) = &r.handle {
        out["handle"] = json!(h);
    }
    if b.url.starts_with("result:") {
        out["from_handle"] = json!(b.url);
    }
    Ok(Json(out))
}

#[derive(Deserialize)]
struct SubmitBody {
    url:  String,
    form: usize,
    #[serde(default)]
    fields: std::collections::BTreeMap<String, String>,
}

/// `POST /submit` — fill + submit a form by registry ordinal.
async fn submit(State(s): State<AppState>, Json(b): Json<SubmitBody>) -> ApiResult {
    let fields: Vec<(String, String)> = b.fields.into_iter().collect();
    let r = s.engine.submit(&b.url, b.form, &fields).await?;
    let mut out = json!({
        "kind": "submit", "source_url": r.source_url, "form": r.form, "action": r.action,
        "method": r.method, "sent": r.sent, "status": r.status, "url": r.page.url,
        "title": r.page.title, "markdown": r.page.markdown, "links": r.page.links,
        "forms": r.page.forms, "salvaged": r.page.salvaged, "js_required": r.page.js_required,
        "content_hash": r.page.content_hash, "cached": r.cached,
    });
    if let Some(h) = &r.handle {
        out["handle"] = json!(h);
    }
    if b.url.starts_with("result:") {
        out["from_handle"] = json!(b.url);
    }
    Ok(Json(out))
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
struct RelatedQ {
    url: String,
    limit: Option<usize>,
}

/// `GET /related?url=…` — a curated page's neighbours by shared entities/tags.
async fn related(State(s): State<AppState>, Query(q): Query<RelatedQ>) -> ApiResult {
    let report = s.engine.related(&q.url, q.limit).await?;
    Ok(Json(json!({
        "kind": "related", "url": report.url, "title": report.title,
        "count": report.related.len(), "related": report.related,
        "distilled_total": report.distilled_total,
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

#[derive(Deserialize)]
struct LogQ {
    limit: Option<usize>,
}

/// `GET /log` — the recent request trail (newest first).
async fn request_log(State(s): State<AppState>, Query(q): Query<LogQ>) -> ApiResult {
    let rows = s.engine.log(q.limit.unwrap_or(20))?;
    Ok(Json(json!({ "kind": "log", "count": rows.len(), "requests": rows })))
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
