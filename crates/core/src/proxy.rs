use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use futures_util::StreamExt;
use rand::seq::SliceRandom;
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::config::Config;
use crate::mesh;
use crate::ollama::{model_matches, LocalTable};
use crate::pool::Pool;
use crate::table::DelegateTable;

/// Request bodies can carry base64 images; allow generous sizes.
const MAX_BODY_BYTES: usize = 256 * 1024 * 1024;

/// Set on requests forwarded between delegates: comma-separated delegate IDs
/// that have already handled the request. A delegate receiving it serves the
/// request itself or rejects it; it never forwards again.
pub const VISITED_HEADER: &str = "x-tokenshare-visited";

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
    /// Ollama instances directly reachable from this machine.
    pub table: LocalTable,
    /// All known delegates, including this one.
    pub delegates: DelegateTable,
    pub pool: Pool,
    pub client: reqwest::Client,
    /// Keepalive message IDs already handled, so relays cannot loop.
    seen_keepalives: Mutex<HashMap<String, Instant>>,
    draining: AtomicBool,
    /// Flips to true to abort every response still streaming.
    cutoff: tokio::sync::watch::Sender<bool>,
}

impl AppState {
    /// `data_dir` is where the delegate table is persisted; `None` keeps it in memory.
    pub fn new(id: String, config: Config, data_dir: Option<PathBuf>) -> Arc<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("http client");
        let delegates = DelegateTable::new(
            id.clone(),
            config.advertise_url.clone(),
            data_dir.map(|d| d.join("table.json")),
        );
        Arc::new(Self {
            id,
            table: LocalTable::new(&config.ollama_hosts),
            delegates,
            pool: Pool::new(config.pool_size),
            config,
            client,
            seen_keepalives: Mutex::new(HashMap::new()),
            draining: AtomicBool::new(false),
            cutoff: tokio::sync::watch::channel(false).0,
        })
    }

    /// Stop taking new model requests (in-flight ones continue).
    pub fn begin_drain(&self) {
        self.draining.store(true, Ordering::SeqCst);
    }

    /// Abort all responses still streaming; clients see a broken stream.
    pub fn cut_off_all(&self) {
        self.cutoff.send_replace(true);
    }

    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::SeqCst)
    }

    /// True the first time `msg_id` is seen. Old IDs are forgotten after 10 minutes.
    pub fn first_sight(&self, msg_id: &str) -> bool {
        let mut seen = self.seen_keepalives.lock().unwrap();
        let now = Instant::now();
        seen.retain(|_, t| now.duration_since(*t) < Duration::from_secs(600));
        seen.insert(msg_id.to_string(), now).is_none()
    }

    /// Re-query local ollama hosts and publish the result in our own entry.
    pub async fn refresh_local(&self) {
        self.table.refresh(&self.client).await;
        // Not sharing means advertising no models, so peers never pick us.
        let models = if self.config.share_with_peers {
            self.table
                .all_models()
                .into_iter()
                .map(|m| m.name)
                .collect()
        } else {
            vec![]
        };
        self.delegates.update_self(models);
    }
}

/// Coordination endpoints live under `/_ts/`; everything else is ollama traffic.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/_ts/status", get(status))
        .route("/_ts/join", post(mesh::join))
        .route("/_ts/announce", post(mesh::announce))
        .route("/_ts/keepalive", post(mesh::keepalive))
        .route("/_ts/table", get(mesh::table))
        .fallback(proxy)
        .with_state(state)
}

/// Serve until `shutdown` resolves.
pub async fn serve(
    listener: tokio::net::TcpListener,
    state: Arc<AppState>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
}

/// Refresh the local ollama table forever.
pub async fn poll_local_table(state: Arc<AppState>) {
    let mut tick =
        tokio::time::interval(Duration::from_secs(state.config.poll_interval_secs.max(1)));
    loop {
        tick.tick().await;
        state.refresh_local().await;
    }
}

async fn status(State(s): State<Arc<AppState>>) -> Json<Value> {
    Json(crate::status::status_json(&s))
}

fn error(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

fn busy() -> Response {
    let mut r = error(
        StatusCode::SERVICE_UNAVAILABLE,
        "all ollama connections are busy",
    );
    r.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    r
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

fn visited_ids(headers: &HeaderMap) -> Vec<String> {
    headers
        .get(VISITED_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// What we send upstream (to local ollama or to a peer delegate).
struct Outbound<'a> {
    client: &'a reqwest::Client,
    method: &'a Method,
    headers: &'a HeaderMap,
    body: &'a Bytes,
}

impl Outbound<'_> {
    async fn send(&self, url: String, visited: Option<&str>) -> reqwest::Result<reqwest::Response> {
        let mut headers = copy_headers(self.headers);
        if let Some(v) = visited {
            headers.insert(
                VISITED_HEADER,
                HeaderValue::from_str(v).expect("ids are ascii"),
            );
        }
        self.client
            .request(self.method.clone(), url)
            .headers(headers)
            .body(self.body.clone())
            .send()
            .await
    }
}

/// Stream an upstream response to the client without buffering it. The pool
/// permit, if any, rides in the stream so the slot is held until the response
/// ends or the client disconnects (dropping the body drops the upstream
/// connection too, which makes ollama stop generating). If the delegate is cut
/// off during shutdown, the stream ends with an error so the client never
/// mistakes a truncated response for a complete one.
fn stream_response<H: Send + 'static>(
    resp: reqwest::Response,
    held: H,
    cutoff: tokio::sync::watch::Receiver<bool>,
) -> Response {
    let status = resp.status();
    let headers = copy_headers(resp.headers());
    let inner = resp
        .bytes_stream()
        .map(|r| r.map_err(axum::BoxError::from))
        .boxed();
    let stream = futures_util::stream::unfold(
        (inner, cutoff, held, false),
        |(mut inner, cutoff, held, done)| async move {
            if done {
                return None;
            }
            // The waiter borrows a clone, so the state tuple can move freely.
            let mut waiter = cutoff.clone();
            let cut = tokio::select! {
                biased;
                _ = waiter.wait_for(|v| *v) => None,
                next = inner.next() => Some(next),
            };
            match cut {
                None => {
                    let err = axum::BoxError::from("delegate shut down mid-response");
                    Some((Err(err), (inner, cutoff, held, true)))
                }
                Some(next) => next.map(|item| (item, (inner, cutoff, held, false))),
            }
        },
    );
    let mut out = Response::new(Body::from_stream(stream));
    *out.status_mut() = status;
    *out.headers_mut() = headers;
    out
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
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or(&path)
        .to_string();
    let out = Outbound {
        client: &s.client,
        method: &parts.method,
        headers: &parts.headers,
        body: &bytes,
    };

    if !INFERENCE_PATHS.contains(&path.as_str()) {
        // Everything else (pull, show, version, ...) is local ollama only.
        let Some(host) = s.table.first_up() else {
            return error(StatusCode::SERVICE_UNAVAILABLE, "no local ollama available");
        };
        return match out.send(format!("{host}{path_and_query}"), None).await {
            Ok(resp) => stream_response(resp, (), s.cutoff.subscribe()),
            Err(e) => {
                warn!(path, host, error = %e, "ollama request failed");
                error(StatusCode::BAD_GATEWAY, "ollama request failed")
            }
        };
    }

    if s.is_draining() {
        let mut r = error(StatusCode::SERVICE_UNAVAILABLE, "delegate is shutting down");
        r.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        return r;
    }

    let Some(model) = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|v| v["model"].as_str().map(str::to_string))
    else {
        return error(StatusCode::BAD_REQUEST, "request has no model");
    };
    let mut visited = visited_ids(&parts.headers);
    let forwarded = !visited.is_empty();
    if forwarded && !s.config.share_with_peers {
        // A peer's view of us can be stale (it may still list our models), so
        // refuse here too. It treats this like any other busy peer and moves on.
        info!(path, model, status = 503, "not sharing with peers");
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "this delegate is not sharing its ollama",
        );
    }
    let log = |target: &str, status: u16| {
        info!(
            method = %parts.method, path, model, target, status, forwarded,
            ttfb_ms = start.elapsed().as_millis() as u64, "request"
        );
    };

    // 1. Serve locally if we have the model and a free pool slot.
    let local_host = s.table.host_for_model(&model);
    if let Some(host) = &local_host {
        if let Some(permit) = s.pool.try_acquire() {
            return match out.send(format!("{host}{path_and_query}"), None).await {
                Ok(resp) => {
                    log(host, resp.status().as_u16());
                    stream_response(resp, permit, s.cutoff.subscribe())
                }
                Err(e) => {
                    warn!(path, host, error = %e, "ollama request failed");
                    error(StatusCode::BAD_GATEWAY, "ollama request failed")
                }
            };
        }
    }

    // 2. Otherwise hand it to a peer, unless we were forwarded this ourselves.
    let mut candidates_exist = local_host.is_some();
    if !forwarded {
        visited.push(s.id.clone());
        let visited_header = visited.join(",");
        let mut peers = s.delegates.peers_with_model(&model, &visited);
        peers.shuffle(&mut rand::rng());
        candidates_exist |= !peers.is_empty();
        // Counts as in-flight from the first attempt until the stream ends.
        let mut guard = Some(s.pool.track_forward());
        for peer in peers {
            // Retrying is safe only until the first byte reaches the client.
            match out
                .send(
                    format!("{}{path_and_query}", peer.url),
                    Some(&visited_header),
                )
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    log(&peer.url, resp.status().as_u16());
                    return stream_response(resp, guard.take(), s.cutoff.subscribe());
                }
                Ok(resp) => {
                    info!(peer = %peer.url, status = resp.status().as_u16(), "peer declined, trying next")
                }
                Err(e) => {
                    warn!(peer = %peer.url, error = %e, "peer unreachable, trying next");
                    if e.is_connect() {
                        s.delegates
                            .set_status(&peer.id, crate::table::Status::Unavailable);
                    }
                }
            }
        }
    }

    if candidates_exist {
        log("-", 503);
        busy()
    } else {
        log("-", 404);
        error(StatusCode::NOT_FOUND, format!("model '{model}' not found"))
    }
}

/// Union of models across this machine and all online peers.
fn tags(s: &AppState) -> Response {
    let mut models: Vec<Value> = s.table.all_models().into_iter().map(|m| m.raw).collect();
    let mut have: Vec<String> = models
        .iter()
        .filter_map(|m| m["name"].as_str().map(str::to_string))
        .collect();
    for peer in s
        .delegates
        .peers()
        .into_iter()
        .filter(|p| p.status == crate::table::Status::Online)
    {
        for name in peer.models {
            if !have.iter().any(|h| model_matches(&name, h)) {
                models.push(json!({ "name": name, "model": name }));
                have.push(name);
            }
        }
    }
    Json(json!({ "models": models })).into_response()
}
