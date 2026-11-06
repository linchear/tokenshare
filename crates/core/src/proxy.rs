use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::activity::{Activity, Refusal, Rejection};
use crate::auth::Secret;
use crate::config::{Config, Face};
use crate::mesh;
use crate::ollama::{model_matches, peer_model_entry, LocalTable};
use crate::pool::Pool;
use crate::routing;
use crate::streamlog::{Kind, Outcome, StreamMeta, StreamTracker};
use crate::table::{DelegateEntry, DelegateTable};

/// The ollama version reported on `/api/version` when there is no local ollama to
/// ask (a client-only delegate, or its ollama is down). With one, its answer is
/// passed through. Clients mostly only check that this is a plausible version.
const ADVERTISED_VERSION: &str = "0.12.0";

/// Request bodies can carry base64 images; allow generous sizes.
const MAX_BODY_BYTES: usize = 256 * 1024 * 1024;

/// Set on requests forwarded between delegates: comma-separated delegate IDs
/// that have already handled the request. A delegate receiving it serves the
/// request itself or rejects it; it never forwards again.
pub const VISITED_HEADER: &str = "x-tokenshare-visited";
/// On every response from an ollama: the name of the delegate whose ollama
/// produced it (its id if it has no name). Set by the delegate the client
/// talked to, so it is there whatever version the serving peer runs.
pub const SERVED_BY_HEADER: &str = "x-tokenshare-served-by";

/// `who` as a header value: printable ASCII as is, anything else (a name can
/// be "Zoë's Mac") percent-encoded as UTF-8, as is `%` itself.
fn served_by(who: &DelegateEntry) -> HeaderValue {
    use std::fmt::Write;
    let text = who.name.as_deref().unwrap_or(&who.id);
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if (0x20..0x7f).contains(&b) && b != b'%' {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    HeaderValue::from_str(&out).unwrap_or(HeaderValue::from_static("unknown"))
}

fn with_served_by(mut r: Response, who: &DelegateEntry) -> Response {
    r.headers_mut().insert(SERVED_BY_HEADER, served_by(who));
    r
}

/// How often a request waiting for a slot asks peers again.
const PEER_RETRY: Duration = Duration::from_secs(2);
/// The most requests waiting for a slot at once; any more are answered 503.
const MAX_WAITING: usize = 64;

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
    /// What this delegate has seen of its upstreams, for routing and the dashboard.
    pub activity: Arc<Activity>,
    pub client: reqwest::Client,
    /// The pool secret, if the pool uses one (see [`crate::auth`]).
    pub secret: Option<Secret>,
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
        )
        .with_expiry_secs(config.peer_expiry_secs);
        delegates.set_self_name(config.display_name());
        // `daemon::start` checks the config first, with a friendlier error.
        let secret = config
            .secret()
            .map(|s| Secret::new(s).expect("pool_secret is invalid"));
        Arc::new(Self {
            secret,
            id,
            table: LocalTable::new(&config.ollama_hosts),
            delegates,
            pool: Pool::with_peer_limit(config.pool_size, config.peer_slots()),
            activity: Activity::new(),
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

    /// `rb`, a request to another delegate, with the pool secret if there is one.
    pub fn to_peer(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.secret {
            Some(s) => rb.header(header::AUTHORIZATION, s.header().clone()),
            None => rb,
        }
    }

    /// Whether other delegates may use the local model `requested` names.
    pub fn lends(&self, requested: &str) -> bool {
        self.table
            .all_models()
            .iter()
            .any(|m| model_matches(requested, &m.name) && self.config.shares_model(&m.name))
    }

    /// Re-query local ollama hosts and publish the result in our own entry.
    pub async fn refresh_local(&self) {
        self.table.refresh(&self.client).await;
        // Peers only hear of the models they may use, so they never pick us
        // for the others (and not sharing at all means advertising none).
        let shared: Vec<_> = self
            .table
            .all_models()
            .into_iter()
            .filter(|m| self.config.shares_model(&m.name))
            .collect();
        let info = shared.iter().map(|m| m.meta()).collect();
        let models = shared.into_iter().map(|m| m.name).collect();
        self.delegates.update_self_with_info(models, info);
        self.publish_load();
    }

    /// Put our current capacity and warm models in our own entry, so the next
    /// keepalive or table pull carries them. A delegate that does not share
    /// publishes none: peers never route to it anyway. Peers see the slots they
    /// may use, so with `max_peer_requests` (or local users filling the pool)
    /// they count us as full while slots are still free for local users.
    pub fn publish_load(&self) {
        if self.config.shares_models() && !self.config.ollama_hosts.is_empty() {
            let slots = self.pool.peer_size();
            let loaded = self
                .table
                .loaded_models()
                .into_iter()
                .filter(|m| self.config.shares_model(m))
                .collect();
            self.delegates.set_self_load(
                Some(slots as u32),
                Some(slots.saturating_sub(self.pool.peer_free()) as u32),
                loaded,
            );
        } else {
            self.delegates.set_self_load(None, None, vec![]);
        }
    }
}

/// Coordination endpoints live under `/_ts/`; everything else is ollama traffic.
/// The router serves the full ollama API; see [`router_for`] for a mesh-only one.
pub fn router(state: Arc<AppState>) -> Router {
    router_for(state, Face::Ollama)
}

/// A router for a listener with the given [`Face`]. Handlers read the face from
/// a request extension, so one `AppState` can sit behind several ports.
pub fn router_for(state: Arc<AppState>, face: Face) -> Router {
    Router::new()
        .route("/_ts/status", get(status))
        .route("/_ts/dashboard", get(dashboard))
        .route("/_ts/metrics", get(metrics))
        .route("/_ts/join", post(mesh::join))
        .route("/_ts/announce", post(mesh::announce))
        .route("/_ts/keepalive", post(mesh::keepalive))
        .route("/_ts/table", get(mesh::table))
        .route("/_ts/forget", post(mesh::forget))
        .fallback(proxy)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::require,
        ))
        .layer(axum::Extension(face))
        .with_state(state)
}

/// Serve the full ollama API until `shutdown` resolves.
pub async fn serve(
    listener: tokio::net::TcpListener,
    state: Arc<AppState>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    serve_face(listener, state, Face::Ollama, shutdown).await
}

/// Serve with the given [`Face`] until `shutdown` resolves.
pub async fn serve_face(
    listener: tokio::net::TcpListener,
    state: Arc<AppState>,
    face: Face,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(
        listener,
        router_for(state, face).into_make_service_with_connect_info::<SocketAddr>(),
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

/// The status, plus whether the caller may manage this delegate (forget
/// peers): only from this machine, as with `/_ts/forget` itself.
async fn status(
    State(s): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Json<Value> {
    let mut v = crate::status::status_json(&s);
    v["manage"] = json!(mesh::is_local(addr.ip()));
    Json(v)
}

/// A page that polls `/_ts/status` and shows the pool. It carries no data of its
/// own, so it shows exactly what `/_ts/status` does to anyone who can reach it.
async fn dashboard() -> Html<&'static str> {
    Html(include_str!("dashboard.html"))
}

/// Prometheus text format; see [`crate::metrics`].
async fn metrics(State(s): State<Arc<AppState>>) -> Response {
    (
        [(header::CONTENT_TYPE, crate::metrics::CONTENT_TYPE)],
        crate::metrics::render(&s),
    )
        .into_response()
}

fn error(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

fn shutting_down() -> Response {
    let mut r = error(StatusCode::SERVICE_UNAVAILABLE, "delegate is shutting down");
    r.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    r
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
    /// Sent to peers in place of whatever `Authorization` the client sent.
    peer_auth: Option<&'a HeaderValue>,
}

impl Outbound<'_> {
    /// Send to `url`: a peer delegate when `visited` is set, else a local ollama.
    async fn send(&self, url: String, visited: Option<&str>) -> reqwest::Result<reqwest::Response> {
        let mut headers = copy_headers(self.headers);
        if let Some(v) = visited {
            headers.insert(
                VISITED_HEADER,
                HeaderValue::from_str(v).expect("ids are ascii"),
            );
            if let Some(auth) = self.peer_auth {
                headers.insert(header::AUTHORIZATION, auth.clone());
            }
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
/// mistakes a truncated response for a complete one. How the response ended is
/// logged once, when the stream finishes or is dropped (see [`streamlog`]).
fn stream_response<H: Send + 'static>(
    resp: reqwest::Response,
    held: H,
    cutoff: tokio::sync::watch::Receiver<bool>,
    mut meta: StreamMeta,
) -> Response {
    let status = resp.status();
    let headers = copy_headers(resp.headers());
    meta.status = status.as_u16();
    meta.kind = Kind::for_response(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        &meta.path,
    );
    meta.ttfb_ms = meta.started.elapsed().as_millis() as u64;
    let tracker = StreamTracker::new(meta);
    let inner = resp
        .bytes_stream()
        .map(|r| r.map_err(axum::BoxError::from))
        .boxed();
    let stream = futures_util::stream::unfold(
        (inner, cutoff, held, false, tracker),
        |(mut inner, cutoff, held, done, mut tracker)| async move {
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
                    tracker.finish(Outcome::CutOff);
                    let err = axum::BoxError::from("delegate shut down mid-response");
                    Some((Err(err), (inner, cutoff, held, true, tracker)))
                }
                Some(None) => {
                    tracker.finish(Outcome::Complete);
                    None
                }
                Some(Some(Ok(chunk))) => {
                    tracker.feed(&chunk);
                    Some((Ok(chunk), (inner, cutoff, held, false, tracker)))
                }
                Some(Some(Err(e))) => {
                    tracker.finish(Outcome::UpstreamError(e.to_string()));
                    Some((Err(e), (inner, cutoff, held, true, tracker)))
                }
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
    // On the tokenshare port of a masquerading delegate, direct clients get a
    // delegate with no models of its own; only other delegates' forwarded
    // requests (below) may use the local ollama hosts.
    let face = parts
        .extensions
        .get::<Face>()
        .copied()
        .unwrap_or(Face::Ollama);
    let on_ollama_port = face == Face::Ollama;

    if parts.method == Method::GET && path == "/api/tags" {
        return tags(&s, on_ollama_port);
    }
    // The ollama CLI and most clients check `HEAD /` (and sometimes /api/version)
    // before anything else, and treat a failure as "no server running": the CLI
    // then tries to launch Ollama.app. A delegate with no local ollama must still
    // answer, so it speaks for itself here instead of passing these through. Both
    // faces do: clients can be pointed at the tokenshare port as well as 11434.
    if parts.method == Method::GET || parts.method == Method::HEAD {
        if path == "/" {
            return (
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                "Ollama is running",
            )
                .into_response();
        }
        // On the ollama port a local ollama's own answer wins; the mesh port has no
        // local ollama for direct clients, so it always answers for itself.
        if path == "/api/version" && (!on_ollama_port || s.table.first_up().is_none()) {
            return Json(json!({ "version": ADVERTISED_VERSION })).into_response();
        }
        // `ollama ps` lists models running on this machine; with no local ollama
        // that is an empty list, not an error.
        if path == "/api/ps" && (!on_ollama_port || s.table.first_up().is_none()) {
            return Json(json!({ "models": [] })).into_response();
        }
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
    // What the end-of-response log line needs to know about this request.
    let meta = |route: &'static str, target: &str, model: Option<&str>| StreamMeta {
        method: parts.method.to_string(),
        path: path.clone(),
        model: model.map(str::to_string),
        route,
        target: target.to_string(),
        request_bytes: bytes.len(),
        started: start,
        status: 0,
        kind: Kind::Other,
        ttfb_ms: 0,
        activity: Some(s.activity.clone()),
    };
    let out = Outbound {
        client: &s.client,
        method: &parts.method,
        headers: &parts.headers,
        body: &bytes,
        peer_auth: s.secret.as_ref().map(Secret::header),
    };

    // `ollama run` looks the model up with /api/show before chatting. It is
    // addressed by model like inference, so it goes wherever the model is, but it
    // is cheap and must not need (or wait for) a pool slot.
    let needs_slot = INFERENCE_PATHS.contains(&path.as_str());
    let model_routed = needs_slot || (path == "/api/show" && parts.method == Method::POST);

    if !model_routed {
        // Everything else (pull, version, ...) is local ollama only.
        let Some(host) = s.table.first_up().filter(|_| on_ollama_port) else {
            return error(StatusCode::SERVICE_UNAVAILABLE, "no local ollama available");
        };
        return match out.send(format!("{host}{path_and_query}"), None).await {
            Ok(resp) => with_served_by(
                stream_response(
                    resp,
                    (),
                    s.cutoff.subscribe(),
                    meta("passthrough", &host, None),
                ),
                &s.delegates.self_entry(),
            ),
            Err(e) => {
                warn!(path, host, error = %e, "ollama request failed");
                error(StatusCode::BAD_GATEWAY, "ollama request failed")
            }
        };
    }

    if s.is_draining() {
        return shutting_down();
    }

    // Older clients send the model as "name"; some (the ollama CLI's `show`) send
    // both with "model" empty, so an empty one does not count.
    let Some(model) = serde_json::from_slice::<Value>(&bytes).ok().and_then(|v| {
        [&v["model"], &v["name"]]
            .into_iter()
            .filter_map(|f| f.as_str())
            .find(|m| !m.is_empty())
            .map(str::to_string)
    }) else {
        return error(StatusCode::BAD_REQUEST, "request has no model");
    };
    let mut visited = visited_ids(&parts.headers);
    let forwarded = !visited.is_empty();
    if forwarded && !s.config.shares_models() {
        // A peer's view of us can be stale (it may still list our models), so
        // refuse here too. It treats this like any other busy peer and moves on.
        info!(path, model, status = 503, "not sharing with peers");
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "this delegate is not sharing its ollama",
        );
    }
    if forwarded && s.table.host_for_model(&model).is_some() && !s.lends(&model) {
        info!(path, model, status = 503, "model not shared with peers");
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "this delegate does not share that model",
        );
    }
    let log = |target: &str, status: u16| {
        info!(
            method = %parts.method, path, model, target, status, forwarded,
            ttfb_ms = start.elapsed().as_millis() as u64, "request"
        );
    };

    // A request from a client waits for a slot when everything is busy; one
    // forwarded by a peer is refused at once, so that peer can try another.
    let wait = if forwarded {
        Duration::ZERO
    } else {
        Duration::from_secs(s.config.queue_timeout_secs)
    };
    let deadline = start + wait;
    if !forwarded {
        visited.push(s.id.clone());
    }
    let visited_header = visited.join(",");
    let mut waiting = None;
    // Peers are asked again only every PEER_RETRY: they cannot tell us when
    // they have room, and asking costs them a refusal each time.
    let mut ask_peers = true;
    loop {
        // Registered before looking, so a slot freed in between still wakes us.
        let freed = s.pool.freed();
        tokio::pin!(freed);
        freed.as_mut().enable();

        // A request that waited into a shutdown must not start now.
        if waiting.is_some() && s.is_draining() {
            log("-", 503);
            return shutting_down();
        }

        // 1. Serve locally if we have the model and a free pool slot.
        // (A forwarded request got here past the sharing check above.)
        let has_local = on_ollama_port || forwarded;
        let local_host = s.table.host_for_model(&model).filter(|_| has_local);
        if let Some(host) = &local_host {
            let permit = if needs_slot {
                s.pool.try_acquire(forwarded).map(Some)
            } else {
                Some(None)
            };
            if let Some(permit) = permit {
                drop(waiting.take());
                let flight = s.activity.start(host);
                return match out.send(format!("{host}{path_and_query}"), None).await {
                    Ok(resp) => {
                        log(host, resp.status().as_u16());
                        with_served_by(
                            stream_response(
                                resp,
                                (permit, flight),
                                s.cutoff.subscribe(),
                                meta("local", host, Some(&model)),
                            ),
                            &s.delegates.self_entry(),
                        )
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
            let peers = routing::order(
                s.delegates.peers_with_model(&model, &visited),
                &model,
                &s.activity,
            );
            candidates_exist |= !peers.is_empty();
            // Counts as in-flight from the first attempt until the stream ends.
            let mut guard = Some(s.pool.track_forward());
            for peer in peers.into_iter().filter(|_| ask_peers) {
                let flight = s.activity.start(&peer.url);
                // Retrying is safe only until the first byte reaches the client.
                match out
                    .send(
                        format!("{}{path_and_query}", peer.url),
                        Some(&visited_header),
                    )
                    .await
                {
                    Ok(resp) if resp.status().is_success() => {
                        drop(waiting.take());
                        log(&peer.url, resp.status().as_u16());
                        s.activity.secret_accepted_by(&peer.url);
                        return with_served_by(
                            stream_response(
                                resp,
                                (guard.take(), flight),
                                s.cutoff.subscribe(),
                                meta("peer", &peer.url, Some(&model)),
                            ),
                            &peer,
                        );
                    }
                    Ok(resp) => {
                        if resp.status() == StatusCode::UNAUTHORIZED {
                            s.activity.secret_refused_by(&peer.url);
                        }
                        s.activity.refused(&peer.url, Refusal::Declined);
                        info!(peer = %peer.url, status = resp.status().as_u16(), "peer declined, trying next")
                    }
                    Err(e) => {
                        s.activity.refused(&peer.url, Refusal::Unreachable);
                        warn!(peer = %peer.url, error = %e, "peer unreachable, trying next");
                        if e.is_connect() {
                            s.delegates
                                .set_status(&peer.id, crate::table::Status::Unavailable);
                        }
                    }
                }
            }
        }

        if !candidates_exist {
            log("-", 404);
            s.activity.rejected(Rejection::NotFound);
            return error(StatusCode::NOT_FOUND, format!("model '{model}' not found"));
        }

        // 3. Everyone who has the model is busy: wait for room, up to the deadline.
        let now = Instant::now();
        let give_up = now >= deadline
            || s.is_draining()
            || (waiting.is_none() && s.pool.waiting() >= MAX_WAITING);
        if give_up {
            log("-", 503);
            s.activity.rejected(Rejection::Busy);
            return busy();
        }
        if waiting.is_none() {
            info!(path, model, "every slot is busy, waiting for one");
            waiting = Some(s.pool.wait_guard());
        }
        tokio::select! {
            _ = &mut freed => ask_peers = false,
            _ = tokio::time::sleep(PEER_RETRY.min(deadline - now)) => ask_peers = true,
        }
    }
}

/// Union of models across this machine and all online peers.
fn tags(s: &AppState, include_local: bool) -> Response {
    let mut models: Vec<Value> = if include_local {
        s.table.all_models().into_iter().map(|m| m.raw).collect()
    } else {
        vec![]
    };
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
                let meta = peer.model_info.iter().find(|m| m.name == name);
                models.push(peer_model_entry(&name, meta, peer.last_seen));
                have.push(name);
            }
        }
    }
    Json(json!({ "models": models })).into_response()
}
