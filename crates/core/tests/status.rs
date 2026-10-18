use serde_json::{json, Value};
use tokenshare_core::mesh::rejoin;

mod common;
use common::{eventually, node, NodeOpts};

async fn state_of(url: &str) -> String {
    let body: Value = reqwest::get(format!("{url}/_ts/status"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    body["state"].as_str().unwrap().to_string()
}

async fn chat(url: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{url}/api/chat"))
        .json(&json!({"model": "gemma2", "messages": []}))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn idle_with_local_ollama_is_active() {
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        ..Default::default()
    })
    .await;
    assert_eq!(state_of(&a.url).await, "active");
}

#[tokio::test]
async fn serving_a_request_is_in_use_until_the_stream_ends() {
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        chunk_delay_ms: 150,
        ..Default::default()
    })
    .await;
    let resp = chat(&a.url).await;
    assert_eq!(state_of(&a.url).await, "in-use");
    resp.text().await.unwrap();
    eventually(3, "back to active", || a.state.pool.active() == 0).await;
    assert_eq!(state_of(&a.url).await, "active");
}

#[tokio::test]
async fn forwarding_a_request_counts_as_in_use() {
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        chunk_delay_ms: 150,
        ..Default::default()
    })
    .await;
    let c = node(NodeOpts {
        fc: Some(&a.url),
        ..Default::default()
    })
    .await;
    assert_eq!(rejoin(&c.state).await, Some(true));

    let resp = chat(&c.url).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(c.state.pool.in_use(), 0, "no local slot used");
    assert_eq!(state_of(&c.url).await, "in-use");
    resp.text().await.unwrap();
    eventually(3, "C back to active", || c.state.pool.active() == 0).await;
    assert_eq!(state_of(&c.url).await, "active");
}

#[tokio::test]
async fn no_ollama_and_no_peers_is_down_but_a_peer_makes_it_active() {
    use tokenshare_core::status::{health, Health};
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        ..Default::default()
    })
    .await;
    let config = tokenshare_core::Config {
        fc: Some(a.url.clone()),
        ollama_hosts: vec!["http://127.0.0.1:1".into()],
        ..Default::default()
    };
    let c = tokenshare_core::AppState::new("c".into(), config, None);
    c.refresh_local().await;
    assert_eq!(health(&c), Health::Down);
    assert_eq!(rejoin(&c).await, Some(true));
    assert_eq!(health(&c), Health::Active);
}
