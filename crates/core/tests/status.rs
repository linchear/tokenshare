use serde_json::{json, Value};
use tokenshare_core::mesh::rejoin;
use tokenshare_core::status::{health, Health};

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

/// A delegate state (not served) with its own ollama on or off.
async fn state_with_local(
    models: &[&str],
    fc: Option<&str>,
) -> std::sync::Arc<tokenshare_core::AppState> {
    let host = if models.is_empty() {
        "http://127.0.0.1:1".to_string() // nothing listens: ollama is off
    } else {
        common::mock_ollama(models, 0).await.url
    };
    let config = tokenshare_core::Config {
        fc: fc.map(str::to_string),
        ollama_hosts: vec![host],
        ..Default::default()
    };
    let s = tokenshare_core::AppState::new(common::uuid_like(), config, None);
    s.refresh_local().await;
    s
}

#[tokio::test]
async fn no_models_anywhere_is_down() {
    // Own ollama off, no peers.
    let c = state_with_local(&[], None).await;
    assert_eq!(health(&c), Health::Down);
}

#[tokio::test]
async fn a_peer_with_models_makes_it_active() {
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        ..Default::default()
    })
    .await;
    let c = state_with_local(&[], Some(&a.url)).await;
    assert_eq!(health(&c), Health::Down);
    assert_eq!(rejoin(&c).await, Some(true));
    assert_eq!(health(&c), Health::Active);
}

#[tokio::test]
async fn online_peers_without_models_are_yellow_not_green_or_red() {
    // The peer's delegate is up but its ollama has nothing to serve.
    let a = node(NodeOpts::default()).await;
    let c = state_with_local(&[], Some(&a.url)).await;
    assert_eq!(rejoin(&c).await, Some(true));
    assert_eq!(c.delegates.peers().len(), 1, "the peer is online");
    assert_eq!(health(&c), Health::NoModels);
}

#[tokio::test]
async fn a_reachable_ollama_with_no_models_is_down() {
    let c = node(NodeOpts::default()).await;
    assert_eq!(health(&c.state), Health::Down);
    assert_eq!(state_of(&c.url).await, "down");
}

#[tokio::test]
async fn own_models_keep_it_active_even_if_peers_have_none() {
    let a = node(NodeOpts::default()).await;
    let c = state_with_local(&["gemma2:latest"], Some(&a.url)).await;
    assert_eq!(rejoin(&c).await, Some(true));
    assert_eq!(health(&c), Health::Active);
}

#[tokio::test]
async fn it_recovers_when_a_peer_gets_models() {
    let a = node(NodeOpts::default()).await;
    let c = state_with_local(&[], Some(&a.url)).await;
    assert_eq!(rejoin(&c).await, Some(true));
    assert_eq!(health(&c), Health::NoModels);
    // A's ollama comes back with a model; C hears about it on its next table sync.
    // (`last_seen` has millisecond resolution; a merge only takes a strictly later one.)
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    a.state.delegates.update_self(vec!["gemma2:latest".into()]);
    assert!(tokenshare_core::mesh::sync_once(&c).await);
    assert_eq!(health(&c), Health::Active);
}

#[tokio::test]
async fn yellow_shows_in_the_status_endpoint_and_turns_red_when_the_peer_goes_away() {
    let mut a = node(NodeOpts::default()).await;
    let c = node(NodeOpts {
        fc: Some(&a.url),
        ..Default::default()
    })
    .await;
    assert_eq!(rejoin(&c.state).await, Some(true));
    assert_eq!(state_of(&c.url).await, "no-models");

    // The only other delegate stops answering: nobody is left, so red.
    a.kill().await;
    for _ in 0..30 {
        tokenshare_core::mesh::send_keepalive(&c.state).await;
        if health(&c.state) == Health::Down {
            break;
        }
    }
    assert_eq!(state_of(&c.url).await, "down");
}
