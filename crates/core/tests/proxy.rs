use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

mod common;
use common::{node, NodeOpts};

async fn delegate(
    models: &[&str],
    pool: usize,
    chunk_delay_ms: u64,
) -> (String, Arc<tokenshare_core::AppState>) {
    let n = node(NodeOpts {
        models,
        pool,
        chunk_delay_ms,
        ..Default::default()
    })
    .await;
    (n.url, n.state)
}

fn chat(model: &str) -> Value {
    json!({"model": model, "messages": []})
}

#[tokio::test]
async fn proxies_and_streams_response() {
    let (url, _) = delegate(&["gemma2:latest"], 3, 0).await;
    let resp = reqwest::Client::new()
        .post(format!("{url}/api/chat"))
        .json(&chat("gemma2"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    let lines: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[2]["done"], true);
}

#[tokio::test]
async fn unknown_model_is_404() {
    let (url, _) = delegate(&["gemma2:latest"], 3, 0).await;
    let resp = reqwest::Client::new()
        .post(format!("{url}/api/chat"))
        .json(&chat("nope"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn missing_model_is_400() {
    let (url, _) = delegate(&["gemma2:latest"], 3, 0).await;
    let resp = reqwest::Client::new()
        .post(format!("{url}/api/chat"))
        .json(&json!({"messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn tags_lists_local_models() {
    let (url, _) = delegate(&["gemma2:latest"], 3, 0).await;
    let body: Value = reqwest::get(format!("{url}/api/tags"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["models"][0]["name"], "gemma2:latest");
}

#[tokio::test]
async fn non_inference_paths_pass_through() {
    let (url, _) = delegate(&["gemma2:latest"], 3, 0).await;
    let body: Value = reqwest::get(format!("{url}/api/version"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["version"], "mock");
}

#[tokio::test]
async fn full_pool_returns_503_and_recovers() {
    let (url, state) = delegate(&["gemma2:latest"], 1, 150).await;
    let client = reqwest::Client::new();

    // First request holds the only slot while its body streams.
    let first = client
        .post(format!("{url}/api/chat"))
        .json(&chat("gemma2"))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    assert_eq!(state.pool.in_use(), 1);

    let second = client
        .post(format!("{url}/api/chat"))
        .json(&chat("gemma2"))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 503);
    assert!(second.headers().contains_key("retry-after"));

    // Drain the first; the slot frees once its body ends.
    first.text().await.unwrap();
    for _ in 0..50 {
        if state.pool.in_use() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let third = client
        .post(format!("{url}/api/chat"))
        .json(&chat("gemma2"))
        .send()
        .await
        .unwrap();
    assert_eq!(third.status(), 200);
}

#[tokio::test]
async fn dropped_client_releases_slot() {
    let (url, state) = delegate(&["gemma2:latest"], 1, 150).await;
    let resp = reqwest::Client::new()
        .post(format!("{url}/api/chat"))
        .json(&chat("gemma2"))
        .send()
        .await
        .unwrap();
    assert_eq!(state.pool.in_use(), 1);
    drop(resp);
    for _ in 0..50 {
        if state.pool.in_use() == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("slot was not released after client disconnect");
}

#[tokio::test]
async fn no_models_means_not_found() {
    let (url, _) = delegate(&[], 3, 0).await;
    let resp = reqwest::Client::new()
        .post(format!("{url}/api/chat"))
        .json(&chat("gemma2"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn status_reports_pool() {
    let (url, state) = delegate(&["gemma2:latest"], 3, 0).await;
    let body: Value = reqwest::get(format!("{url}/_ts/status"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["id"], state.id.as_str());
    assert_eq!(body["pool"]["size"], 3);
    assert_eq!(body["ollama"][0]["up"], true);
}
