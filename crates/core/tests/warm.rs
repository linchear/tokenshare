//! Warm models: preferring a peer that has the model loaded over loading it
//! here (`prefer_warm_peers`), and loading a model ahead of time from the
//! dashboard (`/_ts/warm`).

use reqwest::StatusCode;
use serde_json::{json, Value};
use tokenshare_core::mesh::{pull_table, rejoin};
use tokenshare_core::proxy::SERVED_BY_HEADER;

mod common;
use common::{eventually, node, Node, NodeOpts};

async fn chat(url: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{url}/api/chat"))
        .json(&json!({"model": "gemma2", "messages": []}))
        .send()
        .await
        .unwrap()
}

/// A ("here", with `prefer_warm`) and B ("there"), both with gemma2, B with
/// it loaded and A not. B is in A's table with its load.
async fn pair(prefer_warm: bool, b_pool: usize, b_delay_ms: u64) -> (Node, Node) {
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        name: Some("here"),
        prefer_warm_peers: prefer_warm,
        ..Default::default()
    })
    .await;
    let b = node(NodeOpts {
        models: &["gemma2:latest"],
        name: Some("there"),
        fc: Some(&a.url),
        pool: b_pool,
        chunk_delay_ms: b_delay_ms,
        ..Default::default()
    })
    .await;
    b.mock.set_loaded(&["gemma2:latest"]);
    b.state.refresh_local().await;
    assert_eq!(rejoin(&b.state).await, Some(true));
    eventually(3, "A to list B", || a.state.delegates.peers().len() == 1).await;
    // A recorded B from the join, which carries no load; B's next keepalive
    // would bring it. Only a newer entry is merged, and loopback is fast.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    b.state.delegates.touch_self();
    pull_table(&a.state, &b.url).await.unwrap();
    assert_eq!(a.state.delegates.peers()[0].loaded, vec!["gemma2:latest"]);
    assert!(!a.state.table.is_loaded("gemma2"));
    (a, b)
}

async fn served_by(url: &str) -> String {
    let r = chat(url).await;
    assert_eq!(r.status(), StatusCode::OK);
    let who = r.headers()[SERVED_BY_HEADER].to_str().unwrap().to_string();
    r.text().await.unwrap();
    who
}

#[tokio::test]
async fn by_default_a_cold_local_model_is_still_served_here() {
    let (a, b) = pair(false, 3, 0).await;
    assert_eq!(served_by(&a.url).await, "here");
    assert_eq!(b.mock.chat_count(), 0);
}

#[tokio::test]
async fn with_prefer_warm_peers_a_warm_peer_goes_first() {
    let (a, b) = pair(true, 3, 0).await;
    assert_eq!(served_by(&a.url).await, "there");
    assert_eq!((a.mock.chat_count(), b.mock.chat_count()), (0, 1));
}

#[tokio::test]
async fn a_model_loaded_here_is_served_here_anyway() {
    let (a, b) = pair(true, 3, 0).await;
    a.mock.set_loaded(&["gemma2:latest"]);
    a.state.refresh_local().await;
    assert_eq!(served_by(&a.url).await, "here");
    assert_eq!(b.mock.chat_count(), 0);
}

#[tokio::test]
async fn a_warm_peer_that_is_full_does_not_go_first() {
    // B's only slot is taken by its own user for a while.
    let (a, b) = pair(true, 1, 500).await;
    let held = chat(&b.url).await;
    // B's next keepalive would carry this; a newer entry is what A merges.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    b.state.publish_load();
    b.state.delegates.touch_self();
    pull_table(&a.state, &b.url).await.unwrap();
    let seen = &a.state.delegates.peers()[0];
    assert_eq!(
        (seen.slots, seen.busy),
        (Some(1), Some(1)),
        "A knows B is full"
    );
    assert_eq!(served_by(&a.url).await, "here");
    assert!(
        !a.state.activity.cooling(&b.url),
        "A never asked B, so B never refused"
    );
    drop(held);
}

async fn warm(n: &Node, origin: &str, body: Value) -> (StatusCode, Value) {
    let r = reqwest::Client::new()
        .post(format!("{}/_ts/warm", n.url))
        .header("origin", origin)
        .json(&body)
        .send()
        .await
        .unwrap();
    (r.status(), r.json().await.unwrap_or_default())
}

#[tokio::test]
async fn the_dashboard_can_load_a_model_here() {
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        name: Some("here"),
        ..Default::default()
    })
    .await;
    let (status, body) = warm(&a, &a.url, json!({"model": "gemma2"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["on"], "here");
    let loads = a.mock.loads.lock().unwrap().clone();
    assert_eq!(loads.len(), 1);
    assert_eq!(loads[0]["model"], "gemma2");
    assert_eq!(loads[0]["keep_alive"], "30m");
    assert!(a.state.table.is_loaded("gemma2"), "refreshed straight away");

    let (status, body) = warm(&a, &a.url, json!({"model": "nobody-has-this"})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].as_str().unwrap().contains("does not have"));
}

#[tokio::test]
async fn the_dashboard_can_load_a_model_on_a_peer() {
    let (a, b) = pair(false, 3, 0).await;
    b.mock.set_loaded(&[]);
    let (status, body) = warm(&a, &a.url, json!({"model": "gemma2", "id": b.state.id})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["on"], "there");
    assert_eq!(b.mock.loads.lock().unwrap().len(), 1);
    assert_eq!(a.mock.loads.lock().unwrap().len(), 0);

    let (status, _) = warm(&a, &a.url, json!({"model": "llama3", "id": b.state.id})).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "B does not offer it");
    let (status, _) = warm(&a, &a.url, json!({"model": "gemma2", "id": "nobody"})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn other_web_pages_cannot_load_models() {
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        ..Default::default()
    })
    .await;
    let (status, _) = warm(&a, "http://evil.example", json!({"model": "gemma2"})).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(a.mock.loads.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_peer_that_does_not_lend_the_model_says_so() {
    let a = node(NodeOpts::default()).await;
    // B does not share, but A's (stale) table still says it offers gemma2.
    let b = node(NodeOpts {
        models: &["gemma2:latest"],
        share: false,
        ..Default::default()
    })
    .await;
    let mut stale = b.state.delegates.self_entry();
    stale.url = b.url.clone();
    stale.models = vec!["gemma2:latest".into()];
    stale.last_seen = tokenshare_core::table::now_ms();
    a.state.delegates.merge([stale]);
    let (status, body) = warm(&a, &a.url, json!({"model": "gemma2", "id": b.state.id})).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(body["error"].as_str().unwrap().contains("not sharing"));
    assert!(b.mock.loads.lock().unwrap().is_empty());
}
