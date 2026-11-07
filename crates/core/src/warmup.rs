//! Loading a model ahead of time, on this machine or a peer, from the
//! dashboard: the first real request then starts without waiting for a load.
//!
//! Like forgetting a delegate, this only answers to this machine and to pages
//! this delegate served: it makes peers spend memory, so the rest of the pool
//! and other web pages must not be able to trigger it.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::mesh::{is_local, same_origin};
use crate::ollama::model_matches;
use crate::proxy::{AppState, VISITED_HEADER};
use crate::table::Status;

/// How long a warmed model stays loaded without requests (ollama's
/// `keep_alive`; its own default is 5 minutes).
pub const KEEP_ALIVE: &str = "30m";
/// Big models can take minutes to load from disk.
const LOAD_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Deserialize)]
pub struct WarmRequest {
    pub model: String,
    /// The delegate to load it on; this one if absent.
    #[serde(default)]
    pub id: Option<String>,
}

fn refuse(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

pub async fn warm(
    State(s): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<WarmRequest>,
) -> Response {
    if !is_local(addr.ip()) {
        return refuse(
            StatusCode::FORBIDDEN,
            "loading models is only allowed from this machine",
        );
    }
    if !same_origin(&headers) {
        warn!("refused to load a model for another web page");
        return refuse(
            StatusCode::FORBIDDEN,
            "only this delegate's own dashboard may do that",
        );
    }
    let body =
        json!({ "model": req.model, "prompt": "", "stream": false, "keep_alive": KEEP_ALIVE });
    let started = Instant::now();
    let here = req.id.as_deref().is_none_or(|id| id == s.id);
    let (who, sent) = if here {
        let Some(host) = s.table.host_for_model(&req.model) else {
            return refuse(
                StatusCode::NOT_FOUND,
                format!("this machine does not have {}", req.model),
            );
        };
        let who = s
            .delegates
            .self_entry()
            .name
            .unwrap_or_else(|| s.id.clone());
        let sent = s
            .client
            .post(format!("{host}/api/generate"))
            .timeout(LOAD_TIMEOUT)
            .json(&body)
            .send()
            .await;
        (who, sent)
    } else {
        let id = req.id.unwrap_or_default();
        let Some(peer) = s.delegates.peers().into_iter().find(|p| p.id == id) else {
            return refuse(StatusCode::NOT_FOUND, "no such delegate");
        };
        if peer.status != Status::Online {
            return refuse(StatusCode::CONFLICT, "that delegate is not online");
        }
        if !peer.models.iter().any(|m| model_matches(&req.model, m)) {
            return refuse(
                StatusCode::NOT_FOUND,
                format!("that delegate does not offer {}", req.model),
            );
        }
        // As a forwarded request, so the peer loads it on its own ollama
        // (if it lends it and has a slot) and never passes it on.
        let sent = s
            .to_peer(s.client.post(format!("{}/api/generate", peer.url)))
            .header(VISITED_HEADER, &s.id)
            .timeout(LOAD_TIMEOUT)
            .json(&body)
            .send()
            .await;
        (peer.name.unwrap_or(peer.url), sent)
    };

    let resp = match sent {
        Ok(r) => r,
        Err(e) => {
            warn!(model = %req.model, on = %who, error = %e, "loading a model failed");
            return refuse(
                StatusCode::BAD_GATEWAY,
                format!("{who} did not answer: {e}"),
            );
        }
    };
    let status = resp.status();
    let reply: Value = resp.json().await.unwrap_or_default();
    if !status.is_success() {
        let why = reply["error"].as_str().unwrap_or("it refused").to_string();
        warn!(model = %req.model, on = %who, status = status.as_u16(), why, "loading a model failed");
        let status = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        return refuse(status, format!("{who}: {why}"));
    }
    let ms = started.elapsed().as_millis() as u64;
    info!(model = %req.model, on = %who, ms, "model loaded on request");
    if here {
        // So the dashboard shows it warm right away.
        s.refresh_local().await;
    }
    Json(json!({ "loaded": true, "model": req.model, "on": who, "ms": ms })).into_response()
}
