use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::Json;
use axum::routing::{get, post};
use axum::Router;
use futures_util::stream;
use serde_json::{json, Value};
use tokenshare_core::{router, AppState, Config};
use tokio::net::TcpListener;

async fn serve(app: Router) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}")
}

/// A fake ollama: serves `gemma2:latest`; `/api/chat` streams 3 NDJSON lines.
async fn mock_ollama(chunk_delay_ms: u64) -> String {
    let app = Router::new()
        .route(
            "/api/tags",
            get(|| async { Json(json!({"models": [{"name": "gemma2:latest", "size": 1}]})) }),
        )
        .route(
            "/api/chat",
            post(move |Json(req): Json<Value>| async move {
                let model = req["model"].as_str().unwrap().to_string();
                let s = stream::unfold(0, move |i| {
                    let model = model.clone();
                    async move {
                        if i == 3 {
                            return None;
                        }
                        tokio::time::sleep(Duration::from_millis(chunk_delay_ms)).await;
                        let line =
                            format!("{}\n", json!({"model": model, "chunk": i, "done": i == 2}));
                        Some((Ok::<_, std::io::Error>(line), i + 1))
                    }
                });
                Body::from_stream(s)
            }),
        )
        .route(
            "/api/version",
            get(|| async { Json(json!({"version": "mock"})) }),
        );
    serve(app).await
}

async fn delegate(ollama: &str, pool_size: usize) -> (String, Arc<AppState>) {
    let config = Config {
        ollama_hosts: vec![ollama.to_string()],
        pool_size,
        ..Config::default()
    };
    let state = AppState::new("test-id".into(), config);
    state.table.refresh(&state.client).await;
    (serve(router(state.clone())).await, state)
}

fn chat(model: &str) -> Value {
    json!({"model": model, "messages": []})
}

#[tokio::test]
async fn proxies_and_streams_response() {
    let ollama = mock_ollama(0).await;
    let (url, _) = delegate(&ollama, 3).await;
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
    let ollama = mock_ollama(0).await;
    let (url, _) = delegate(&ollama, 3).await;
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
    let ollama = mock_ollama(0).await;
    let (url, _) = delegate(&ollama, 3).await;
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
    let ollama = mock_ollama(0).await;
    let (url, _) = delegate(&ollama, 3).await;
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
    let ollama = mock_ollama(0).await;
    let (url, _) = delegate(&ollama, 3).await;
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
    let ollama = mock_ollama(150).await;
    let (url, state) = delegate(&ollama, 1).await;
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
    let ollama = mock_ollama(150).await;
    let (url, state) = delegate(&ollama, 1).await;
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
async fn ollama_down_means_model_not_found() {
    let (url, _) = delegate("http://127.0.0.1:1", 3).await;
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
    let ollama = mock_ollama(0).await;
    let (url, _) = delegate(&ollama, 3).await;
    let body: Value = reqwest::get(format!("{url}/_ts/status"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["id"], "test-id");
    assert_eq!(body["pool"]["size"], 3);
    assert_eq!(body["ollama"][0]["up"], true);
}
