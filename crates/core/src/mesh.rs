//! Coordination between delegates: join, announce, table sync.
//! All of it lives under `/_ts/` on the same port as the ollama proxy.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap};
use axum::Json;
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::{debug, info, warn};

use crate::proxy::AppState;
use crate::table::{DelegateEntry, Status};

const COORD_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Serialize, Deserialize)]
pub struct JoinRequest {
    pub id: String,
    /// Our address, if configured. Otherwise the receiver derives it from the
    /// connection plus `port`.
    pub url: Option<String>,
    pub port: u16,
    pub models: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TableResponse {
    pub hash: String,
    pub count: usize,
    pub table: Vec<DelegateEntry>,
    /// Present in join replies: the entry the receiver recorded for the joiner.
    #[serde(rename = "self", default, skip_serializing_if = "Option::is_none")]
    pub joined_as: Option<DelegateEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Announce {
    /// Always `new-member`.
    pub kind: String,
    /// The announcer's URL, for pulling its table on a mismatch.
    pub from: String,
    pub entry: DelegateEntry,
    pub hash: String,
    pub count: usize,
}

fn table_response(s: &AppState, joined_as: Option<DelegateEntry>) -> TableResponse {
    TableResponse {
        hash: s.delegates.hash(),
        count: s.delegates.count(),
        table: s.delegates.shared_entries(),
        joined_as,
    }
}

fn fmt_ip(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => format!("[{v6}]"),
        },
    }
}

// ---- server side ----

/// A delegate introduces itself. We record it, reply with our table, and tell
/// everyone else about it.
pub async fn join(
    State(s): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<JoinRequest>,
) -> Json<TableResponse> {
    // The Host header is how the joiner reached us, which is how others can too.
    if let Some(host) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) {
        s.delegates
            .set_self_url_if_unknown(&format!("http://{host}"));
    }
    let url = req
        .url
        .unwrap_or_else(|| format!("http://{}:{}", fmt_ip(addr.ip()), req.port));
    let entry = DelegateEntry::new(req.id, url, req.models);
    info!(id = %entry.id, url = %entry.url, "delegate joined");
    s.delegates.merge([entry.clone()]);
    tokio::spawn(announce_new_member(s.clone(), entry.clone()));
    Json(table_response(&s, Some(entry)))
}

/// Another delegate says a new member has joined.
pub async fn announce(State(s): State<Arc<AppState>>, Json(msg): Json<Announce>) -> Json<Value> {
    if msg.entry.id != s.id {
        s.delegates.merge([msg.entry]);
    }
    if s.delegates.hash() != msg.hash || s.delegates.count() != msg.count {
        debug!(from = %msg.from, "table drift after announce, pulling full table");
        tokio::spawn(async move {
            if let Err(e) = pull_table(&s, &msg.from).await {
                warn!(from = %msg.from, error = %e, "table pull failed");
            }
        });
    }
    Json(json!({ "ok": true }))
}

pub async fn table(State(s): State<Arc<AppState>>) -> Json<TableResponse> {
    Json(table_response(&s, None))
}

// ---- client side ----

/// Tell every other delegate about `entry`.
async fn announce_new_member(s: Arc<AppState>, entry: DelegateEntry) {
    let from = s.delegates.self_entry().url;
    let msg = Announce {
        kind: "new-member".into(),
        from,
        entry: entry.clone(),
        hash: s.delegates.hash(),
        count: s.delegates.count(),
    };
    let targets = s
        .delegates
        .peers()
        .into_iter()
        .filter(|p| p.id != entry.id && p.status != Status::Inactive);
    let sends = targets.map(|p| {
        let s = &s;
        let msg = &msg;
        async move {
            let r = s
                .client
                .post(format!("{}/_ts/announce", p.url))
                .timeout(COORD_TIMEOUT)
                .json(msg)
                .send()
                .await
                .and_then(|r| r.error_for_status());
            if let Err(e) = r {
                warn!(peer = %p.url, error = %e, "announce failed");
            }
        }
    });
    futures_util::future::join_all(sends).await;
}

/// Fetch `url`'s full table and merge it into ours.
pub async fn pull_table(s: &AppState, url: &str) -> Result<()> {
    let resp: TableResponse = s
        .client
        .get(format!("{url}/_ts/table"))
        .timeout(COORD_TIMEOUT)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    s.delegates.merge(resp.table);
    Ok(())
}

/// Introduce ourselves to the delegate at `url` and merge the table it sends.
pub async fn join_via(s: &AppState, url: &str) -> Result<()> {
    let me = s.delegates.self_entry();
    let req = JoinRequest {
        id: s.id.clone(),
        url: (!me.url.is_empty()).then_some(me.url),
        port: s.config.port,
        models: me.models,
    };
    let resp: TableResponse = s
        .client
        .post(format!("{url}/_ts/join"))
        .timeout(COORD_TIMEOUT)
        .json(&req)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let joined_as = resp
        .joined_as
        .ok_or_else(|| anyhow!("join reply had no self entry"))?;
    s.delegates.set_self_url_if_unknown(&joined_as.url);
    s.delegates.merge(resp.table);
    Ok(())
}

/// Join the mesh. Candidates are the persisted peers in random order, then the
/// first-contact delegate. Any that cannot be reached are marked inactive.
/// Returns whether a delegate accepted us, or `None` if there was nobody to try.
pub async fn rejoin(s: &AppState) -> Option<bool> {
    let mut urls: Vec<String> = s.delegates.peers().into_iter().map(|p| p.url).collect();
    urls.shuffle(&mut rand::rng());
    if let Some(fc) = &s.config.fc {
        let fc = fc.trim_end_matches('/').to_string();
        if !urls.contains(&fc) {
            urls.push(fc);
        }
    }
    if urls.is_empty() {
        return None;
    }
    for url in urls {
        match join_via(s, &url).await {
            Ok(()) => {
                info!(%url, delegates = s.delegates.count(), "joined");
                return Some(true);
            }
            Err(e) => {
                warn!(%url, error = %e, "cannot reach delegate");
                if let Some(id) = s.delegates.id_for_url(&url) {
                    s.delegates.set_status(&id, Status::Inactive);
                }
            }
        }
    }
    Some(false)
}

/// Keep trying to join until a delegate accepts us.
pub async fn join_loop(s: Arc<AppState>) {
    loop {
        match rejoin(&s).await {
            None => {
                info!("no first-contact or known delegates; waiting to be joined");
                return;
            }
            Some(true) => return,
            Some(false) => {
                let wait = s.config.rejoin_retry_secs;
                warn!(wait, "no delegate reachable; will retry");
                tokio::time::sleep(Duration::from_secs(wait)).await;
            }
        }
    }
}
