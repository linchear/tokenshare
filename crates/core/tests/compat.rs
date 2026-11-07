//! Delegates of different versions in one pool. People upgrade one machine at a
//! time, so a new delegate must work with a v0.1.0 one in both directions.
//!
//! The "old delegate" here is a fake that speaks exactly the v0.1.0 wire format:
//! its structs are copied from the `v0.1.0` tag, and like v0.1.0 they ignore
//! fields they do not know. If a change makes a new delegate send something
//! v0.1.0 cannot parse, or need something it does not send, these tests fail.

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokenshare_core::mesh::{pull_table, rejoin, send_keepalive};
use tokenshare_core::table::now_ms;

mod common;
use common::{node, serve_plain, NodeOpts};

// ---- the v0.1.0 wire format, as of the `v0.1.0` tag ----

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OldEntry {
    id: String,
    url: String,
    status: String,
    #[serde(rename = "type")]
    kind: String,
    models: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    load: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    vram: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ram: Option<u64>,
    last_seen: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct OldJoin {
    id: String,
    url: Option<String>,
    port: u16,
    models: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OldTable {
    hash: String,
    count: usize,
    table: Vec<OldEntry>,
    #[serde(rename = "self", default, skip_serializing_if = "Option::is_none")]
    joined_as: Option<OldEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OldKeepalive {
    msg_id: String,
    via: String,
    ttl: u8,
    entry: OldEntry,
}

/// v0.1.0's table hash: ids and URLs only, sorted by id.
fn old_hash(entries: &[OldEntry]) -> String {
    let mut sorted: Vec<&OldEntry> = entries.iter().filter(|e| !e.url.is_empty()).collect();
    sorted.sort_by(|a, b| a.id.cmp(&b.id));
    let mut h = Sha256::new();
    for e in sorted {
        h.update(e.id.as_bytes());
        h.update(b"\n");
        h.update(e.url.as_bytes());
        h.update(b"\n");
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

// ---- a fake v0.1.0 delegate ----

#[derive(Default)]
struct Old {
    me: Mutex<Option<OldEntry>>,
    /// Everyone else, as v0.1.0 parsed them.
    known: Mutex<Vec<OldEntry>>,
    chats: Mutex<usize>,
}

impl Old {
    fn table(&self, joined_as: Option<OldEntry>) -> OldTable {
        let mut table = self.known.lock().unwrap().clone();
        table.push(self.me.lock().unwrap().clone().unwrap());
        OldTable {
            hash: old_hash(&table),
            count: table.len(),
            table,
            joined_as,
        }
    }

    fn remember(&self, e: OldEntry) {
        let mut known = self.known.lock().unwrap();
        known.retain(|k| k.id != e.id);
        known.push(e);
    }
}

/// Start the fake: it offers `gemma2:latest` and answers chats itself.
async fn old_delegate() -> (String, Arc<Old>) {
    let old = Arc::new(Old::default());
    let app = Router::new()
        .route(
            "/_ts/join",
            post(
                |State(o): State<Arc<Old>>, Json(req): Json<OldJoin>| async move {
                    let e = OldEntry {
                        id: req.id,
                        url: req.url.unwrap_or_default(),
                        status: "online".into(),
                        kind: "delegate".into(),
                        models: req.models,
                        load: None,
                        vram: None,
                        ram: None,
                        last_seen: now_ms(),
                    };
                    o.remember(e.clone());
                    Json(o.table(Some(e)))
                },
            ),
        )
        .route(
            "/_ts/table",
            get(|State(o): State<Arc<Old>>| async move { Json(o.table(None)) }),
        )
        .route(
            "/_ts/keepalive",
            post(
                |State(o): State<Arc<Old>>, Json(k): Json<OldKeepalive>| async move {
                    o.remember(k.entry);
                    Json(json!({"ok": true}))
                },
            ),
        )
        .route(
            "/api/chat",
            post(|State(o): State<Arc<Old>>| async move {
                *o.chats.lock().unwrap() += 1;
                Json(json!({"model": "gemma2:latest", "done": true}))
            }),
        )
        .with_state(old.clone());
    let url = serve_plain(app).await;
    *old.me.lock().unwrap() = Some(OldEntry {
        id: "00000000-old-delegate".into(),
        url: url.clone(),
        status: "online".into(),
        kind: "delegate".into(),
        models: vec!["gemma2:latest".into()],
        load: None,
        vram: None,
        ram: None,
        last_seen: now_ms(),
    });
    (url, old)
}

// ---- tests ----

#[tokio::test]
async fn a_new_delegate_joins_an_old_one_gossips_and_borrows_its_models() {
    let (old_url, old) = old_delegate().await;
    let new = node(NodeOpts {
        models: &["llama3:latest"],
        fc: Some(&old_url),
        ..Default::default()
    })
    .await;
    assert_eq!(rejoin(&new.state).await, Some(true));

    // The old delegate publishes no load; the new one copes.
    let seen = new.state.delegates.peers();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].models, vec!["gemma2:latest".to_string()]);
    assert_eq!((seen[0].slots, seen[0].busy), (None, None));
    assert!(seen[0].loaded.is_empty());
    assert_eq!(seen[0].name, None, "nor a name");
    assert_eq!(
        (&seen[0].version, &seen[0].ollama_version),
        (&None, &None),
        "nor versions"
    );

    // Its keepalive, load fields and all, parses as v0.1.0's keepalive.
    assert!(send_keepalive(&new.state).await);
    let known = old.known.lock().unwrap().clone();
    let me = known.iter().find(|e| e.id == new.state.id).unwrap();
    assert_eq!(me.models, vec!["llama3:latest".to_string()]);
    pull_table(&new.state, &old_url).await.unwrap();

    // Routing works without any load information for the peer.
    let r = reqwest::Client::new()
        .post(format!("{}/api/chat", new.url))
        .json(&json!({"model": "gemma2", "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    r.text().await.unwrap();
    assert_eq!(*old.chats.lock().unwrap(), 1);
}

#[tokio::test]
async fn an_old_delegate_can_join_a_new_one_and_read_its_table() {
    let new = node(NodeOpts {
        models: &["gemma2:latest"],
        name: Some("new-one"),
        ..Default::default()
    })
    .await;
    let client = reqwest::Client::new();

    // Exactly what v0.1.0 sends to join.
    let join = OldJoin {
        id: "00000000-old-delegate".into(),
        url: Some("http://127.0.0.1:9".into()),
        port: 9,
        models: vec!["mistral:latest".into()],
    };
    let reply: OldTable = client
        .post(format!("{}/_ts/join", new.url))
        .json(&join)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .expect("v0.1.0 can parse the join reply");
    assert_eq!(reply.joined_as.unwrap().url, "http://127.0.0.1:9");

    // The new delegate's own entry carries load fields v0.1.0 ignores.
    let raw: Value = client
        .get(format!("{}/_ts/table", new.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let mine = raw["table"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == new.state.id.as_str())
        .unwrap();
    assert!(mine["slots"].is_number(), "the new fields are really there");
    assert_eq!(mine["name"], "new-one");
    assert_eq!(mine["model_info"][0]["name"], "gemma2:latest");
    assert_eq!(mine["version"], env!("CARGO_PKG_VERSION"));
    let table: OldTable = serde_json::from_value(raw).expect("v0.1.0 can parse the table");
    assert_eq!(table.count, 2);
    // Both versions hash the same table the same way, so announces do not
    // trigger endless table pulls between them.
    assert_eq!(table.hash, old_hash(&table.table));

    // A v0.1.0 keepalive is accepted and keeps the old delegate online.
    let ka = OldKeepalive {
        msg_id: "old-keepalive-1".into(),
        via: "00000000-old-delegate".into(),
        ttl: 0,
        entry: OldEntry {
            id: "00000000-old-delegate".into(),
            url: "http://127.0.0.1:9".into(),
            status: "online".into(),
            kind: "delegate".into(),
            models: vec!["mistral:latest".into(), "phi3:latest".into()],
            load: None,
            vram: None,
            ram: None,
            last_seen: now_ms() + 1,
        },
    };
    let r = client
        .post(format!("{}/_ts/keepalive", new.url))
        .json(&ka)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let old_seen = new
        .state
        .delegates
        .peers()
        .into_iter()
        .find(|p| p.id == "00000000-old-delegate")
        .unwrap();
    assert_eq!(old_seen.models.len(), 2);
    assert_eq!(old_seen.slots, None);
    assert_eq!(old_seen.name, None, "v0.1.0 sends no name");
}
