use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokenshare_core::daemon::{self, Running};
use tokenshare_core::Config;

mod common;
use common::mock_ollama;

async fn start(chunk_delay_ms: u64) -> (Running, String) {
    let mock = mock_ollama(&["gemma2:latest"], chunk_delay_ms).await;
    let dir = std::env::temp_dir().join(format!("ts-shutdown-{}", common::uuid_like()));
    let config = Config {
        port: 0,
        ollama_hosts: vec![mock.url.clone()],
        ..Config::default()
    };
    let running = daemon::start(config, dir).await.unwrap();
    let url = format!("http://127.0.0.1:{}", running.addr.port());
    (running, url)
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
async fn idle_stop_is_quick() {
    let (running, url) = start(0).await;
    let t = Instant::now();
    running.stop(Duration::from_secs(30)).await.unwrap();
    assert!(t.elapsed() < Duration::from_secs(2));
    assert!(reqwest::get(format!("{url}/_ts/status")).await.is_err());
}

#[tokio::test]
async fn drain_lets_in_flight_streams_finish_and_refuses_new_requests() {
    let (running, url) = start(200).await;
    let mut resp = chat(&url).await;
    let first = resp.chunk().await.unwrap().unwrap();

    let state = running.state.clone();
    let stopper = tokio::spawn(running.stop(Duration::from_secs(10)));
    common::eventually(2, "draining to start", || state.is_draining()).await;

    // New model requests are turned away while the old one continues.
    let refused = chat(&url).await;
    assert_eq!(refused.status(), 503);
    assert!(refused.headers().contains_key("retry-after"));
    let status: Value = reqwest::get(format!("{url}/_ts/status"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["draining"], true);

    // The in-flight response arrives complete.
    let mut text = String::from_utf8(first.to_vec()).unwrap();
    while let Some(c) = resp.chunk().await.unwrap() {
        text.push_str(std::str::from_utf8(&c).unwrap());
    }
    assert_eq!(text.lines().count(), 3);
    assert!(text.lines().last().unwrap().contains("\"done\":true"));

    // Only then does stop return, and the port closes.
    stopper.await.unwrap().unwrap();
    assert!(reqwest::get(format!("{url}/_ts/status")).await.is_err());
}

#[tokio::test]
async fn requests_still_running_after_the_grace_period_are_cut_off() {
    let (running, url) = start(1000).await;
    let mut resp = chat(&url).await;
    resp.chunk().await.unwrap().unwrap();

    let t = Instant::now();
    running.stop(Duration::from_millis(300)).await.unwrap();
    assert!(
        t.elapsed() < Duration::from_secs(2),
        "stop took {:?}",
        t.elapsed()
    );

    // The client sees an error, not a stream that merely ends early.
    let mut chunks = 1;
    let mut failed = false;
    loop {
        match resp.chunk().await {
            Ok(Some(_)) => chunks += 1,
            Ok(None) => break,
            Err(_) => {
                failed = true;
                break;
            }
        }
    }
    assert!(
        failed,
        "stream ended cleanly after {chunks} chunks instead of erroring"
    );
}
