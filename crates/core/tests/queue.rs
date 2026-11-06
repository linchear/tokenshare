//! When every slot that could serve a request is busy, a client's request
//! waits for one (up to `queue_timeout_secs`) instead of failing at once.

use std::time::{Duration, Instant};

use reqwest::StatusCode;
use serde_json::{json, Value};
use tokenshare_core::mesh::{pull_table, rejoin};
use tokenshare_core::proxy::{SERVED_BY_HEADER, VISITED_HEADER};

mod common;
use common::{eventually, node, NodeOpts};

async fn chat(url: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{url}/api/chat"))
        .json(&json!({"model": "gemma2", "messages": []}))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_request_waits_for_a_local_slot() {
    // Each response holds the only slot for about 3 x 200 ms.
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        pool: 1,
        chunk_delay_ms: 200,
        queue_timeout_secs: 10,
        ..Default::default()
    })
    .await;
    let first = chat(&a.url).await;
    assert_eq!(first.status(), StatusCode::OK);

    let started = Instant::now();
    let second = tokio::spawn({
        let url = a.url.clone();
        async move { chat(&url).await }
    });
    eventually(3, "the second request to wait", || {
        a.state.pool.waiting() == 1
    })
    .await;
    let status: Value = reqwest::get(format!("{}/_ts/status", a.url))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["pool"]["waiting"], 1);
    let metrics = reqwest::get(format!("{}/_ts/metrics", a.url))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains("\ntokenshare_requests_waiting 1\n"),
        "{metrics}"
    );

    first.text().await.unwrap();
    let second = second.await.unwrap();
    assert_eq!(
        second.status(),
        StatusCode::OK,
        "served once the slot freed"
    );
    assert!(started.elapsed() >= Duration::from_millis(200));
    second.text().await.unwrap();
    assert_eq!(a.state.pool.waiting(), 0);
    assert_eq!(a.mock.chat_count(), 2);
}

#[tokio::test]
async fn a_request_that_waits_too_long_gets_503() {
    // The slot stays taken for about 4.5 s; the queue gives up after 1.
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        pool: 1,
        chunk_delay_ms: 1500,
        queue_timeout_secs: 1,
        ..Default::default()
    })
    .await;
    let held = chat(&a.url).await;
    let started = Instant::now();
    let r = chat(&a.url).await;
    let waited = started.elapsed();
    assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(r.headers().contains_key("retry-after"));
    assert!(
        waited >= Duration::from_secs(1) && waited < Duration::from_secs(3),
        "{waited:?}"
    );
    assert_eq!(a.state.pool.waiting(), 0);
    assert_eq!(a.mock.chat_count(), 1);
    drop(held);
}

#[tokio::test]
async fn a_request_waits_for_a_peer_slot() {
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        pool: 1,
        chunk_delay_ms: 300,
        name: Some("gpu-box"),
        ..Default::default()
    })
    .await;
    let c = node(NodeOpts {
        fc: Some(&a.url),
        client_only: true,
        queue_timeout_secs: 10,
        ..Default::default()
    })
    .await;
    assert_eq!(rejoin(&c.state).await, Some(true));
    a.state.refresh_local().await;
    pull_table(&c.state, &a.url).await.unwrap();

    // A's own user takes its only slot; C's request is refused by A at first,
    // then served when C asks again.
    let held = chat(&a.url).await;
    let r = chat(&c.url).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()[SERVED_BY_HEADER], "gpu-box");
    r.text().await.unwrap();
    assert_eq!(a.mock.chat_count(), 2);
    drop(held);
}

#[tokio::test]
async fn a_request_forwarded_by_a_peer_does_not_wait() {
    // The peer waits instead, and meanwhile may find room elsewhere.
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        pool: 1,
        chunk_delay_ms: 1500,
        queue_timeout_secs: 10,
        ..Default::default()
    })
    .await;
    let held = chat(&a.url).await;
    let started = Instant::now();
    let r = reqwest::Client::new()
        .post(format!("{}/api/chat", a.url))
        .header(VISITED_HEADER, "some-other-delegate")
        .json(&json!({"model": "gemma2", "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(started.elapsed() < Duration::from_secs(1));
    drop(held);
}

#[tokio::test]
async fn an_unknown_model_is_404_at_once() {
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        queue_timeout_secs: 10,
        ..Default::default()
    })
    .await;
    let started = Instant::now();
    let r = reqwest::Client::new()
        .post(format!("{}/api/chat", a.url))
        .json(&json!({"model": "nobody-has-this", "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn waiting_requests_are_refused_when_the_delegate_shuts_down() {
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        pool: 1,
        chunk_delay_ms: 300,
        queue_timeout_secs: 30,
        ..Default::default()
    })
    .await;
    let held = chat(&a.url).await;
    let waiter = tokio::spawn({
        let url = a.url.clone();
        async move { chat(&url).await }
    });
    eventually(3, "the request to wait", || a.state.pool.waiting() == 1).await;
    a.state.begin_drain();
    // The held response ends and frees its slot; the waiter must not take it.
    held.text().await.unwrap();
    let r = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("answered within a retry interval")
        .unwrap();
    assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = r.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("shutting down"));
    assert_eq!(a.mock.chat_count(), 1);
}
