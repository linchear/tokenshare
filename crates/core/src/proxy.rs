use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderName, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::StreamExt;
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::config::Config;
use crate::ollama::LocalTable;
use crate::pool::Pool;

/// Request bodies can carry base64 images; allow generous sizes.
const MAX_BODY_BYTES: usize = 256 * 1024 * 1024;

/// Endpoints that run a model and so consume a pool slot.
const INFERENCE_PATHS: &[&str] = &[
    "/api/chat",
    "/api/generate",
    "/api/embed",
    "/api/embeddings",
    "/v1/chat/completions",
    "/v1/completions",
    "/v1/embeddings",
];

pub struct AppState {
    pub id: String,
    pub config: Config,
    pub table: LocalTable,
    pub pool: Pool,
    pub client: reqwest::Client,
}

impl AppState {
    pub fn new(id: String, config: Config) -> Arc<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("http client");
        Arc::new(Self {
            id,
            table: LocalTable::new(&config.ollama_hosts),
            pool: Pool::new(config.pool_size),
            config,
            client,
        })
    }
}

/// Coordination endpoints live under `/_ts/`; everything else is ollama traffic.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/_ts/status", get(status))
        .fallback(proxy)
        .with_state(state)
}

/// Refresh the local ollama table forever.
pub async fn poll_local_table(state: Arc<AppState>) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(
        state.config.poll_interval_secs.max(1),
    ));
    loop {
        tick.tick().await;
        state.table.refresh(&state.client).await;
    }
}

async fn status(State(s): State<Arc<AppState>>) -> Json<Value> {
    let hosts: Vec<Value> = s
        .table
        .snapshot()
        .into_iter()
        .map(|h| {
            json!({
                "host": h.host,
                "up": h.up,
                "models": h.models.iter().map(|m| &m.name).collect::<Vec<_>>(),
            })
        })
        .collect();
    Json(json!({
        "id": s.id,
        "pool": { "size": s.pool.size(), "in_use": s.pool.in_use() },
        "ollama": hosts,
    }))
}

fn error(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "host"
            | "content-length"
            | "connection"
            | "transfer-encoding"
            | "keep-alive"
            | "te"
            | "upgrade"
    )
}

fn copy_headers(from: &HeaderMap) -> HeaderMap {
    from.iter()
        .filter(|(k, _)| !is_hop_by_hop(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

async fn proxy(State(s): State<Arc<AppState>>, req: Request) -> Response {
    let start = Instant::now();
    let (parts, body) = req.into_parts();
    let path = parts.uri.path().to_string();

    if parts.method == Method::GET && path == "/api/tags" {
        return tags(&s);
    }

    let bytes = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(_) => return error(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"),
    };

    let mut model = None;
    let mut permit = None;
    let host = if INFERENCE_PATHS.contains(&path.as_str()) {
        let Some(m) = serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|v| v["model"].as_str().map(str::to_string))
        else {
            return error(StatusCode::BAD_REQUEST, "request has no model");
        };
        let Some(host) = s.table.host_for_model(&m) else {
            info!(path, model = %m, status = 404, "model not available");
            return error(StatusCode::NOT_FOUND, format!("model '{m}' not found"));
        };
        let Some(p) = s.pool.try_acquire() else {
            info!(path, model = %m, status = 503, "pool full");
            let mut r = error(
                StatusCode::SERVICE_UNAVAILABLE,
                "all ollama connections are busy",
            );
            r.headers_mut()
                .insert(header::RETRY_AFTER, "1".parse().unwrap());
            return r;
        };
        permit = Some(p);
        model = Some(m);
        host
    } else {
        match s.table.first_up() {
            Some(h) => h,
            None => return error(StatusCode::SERVICE_UNAVAILABLE, "no ollama host available"),
        }
    };

    let url = format!(
        "{host}{}",
        parts
            .uri
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or(&path)
    );
    let upstream = s
        .client
        .request(parts.method.clone(), url)
        .headers(copy_headers(&parts.headers))
        .body(bytes)
        .send()
        .await;

    let resp = match upstream {
        Ok(r) => r,
        Err(e) => {
            warn!(path, host, error = %e, "ollama request failed");
            return error(StatusCode::BAD_GATEWAY, "ollama request failed");
        }
    };

    let status = resp.status();
    info!(
        method = %parts.method,
        path,
        model = model.as_deref().unwrap_or("-"),
        host,
        status = status.as_u16(),
        ttfb_ms = start.elapsed().as_millis() as u64,
        "request"
    );

    let headers = copy_headers(resp.headers());
    // The permit rides along in the stream so the pool slot is held until the
    // response finishes (or the client disconnects and the body is dropped).
    let stream = resp.bytes_stream().map(move |chunk| {
        let _held = &permit;
        chunk
    });
    let mut out = Response::new(Body::from_stream(stream));
    *out.status_mut() = status;
    *out.headers_mut() = headers;
    out
}

fn tags(s: &AppState) -> Response {
    let all = s.table.all_models();
    Json(json!({ "models": all.iter().map(|m| &m.raw).collect::<Vec<_>>() })).into_response()
}
