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

fn temp_dir() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("ts-start-{}", common::uuid_like()))
}

#[tokio::test]
async fn starting_on_a_busy_port_fails_with_a_clear_message() {
    // Loopback-only, like ollama: binding the wildcard would "succeed" on macOS.
    let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = busy.local_addr().unwrap().port();
    let config = Config {
        port,
        ollama_hosts: vec![],
        ..Config::default()
    };
    let err = daemon::start(config, temp_dir())
        .await
        .err()
        .expect("must fail");
    assert!(
        err.to_string()
            .contains(&format!("port {port} is already in use")),
        "{err:#}"
    );
}

#[tokio::test]
async fn an_ollama_host_that_is_the_delegate_itself_is_refused() {
    let free = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let config = Config {
        port: free,
        ollama_hosts: vec![format!("http://localhost:{free}")],
        ..Config::default()
    };
    let err = daemon::start(config, temp_dir())
        .await
        .err()
        .expect("must fail");
    assert!(
        err.to_string().contains("is this delegate itself"),
        "{err:#}"
    );
}

#[tokio::test]
async fn a_masquerading_delegate_serves_on_both_ports_and_stops_them_both() {
    use tokenshare_core::config::{Face, Listener};
    let mock = mock_ollama(&["gemma2:latest"], 0).await;
    let dir = std::env::temp_dir().join(format!("ts-masq-{}", common::uuid_like()));
    let config = Config {
        port: 0,
        masquerade: true,
        ollama_hosts: vec![mock.url.clone()],
        ..Config::default()
    };
    let listeners = vec![
        Listener {
            port: 0,
            face: Face::Mesh,
        },
        Listener {
            port: 0,
            face: Face::Ollama,
        },
    ];
    let running = daemon::start_on(config, dir, listeners).await.unwrap();
    let ports = running.ports();
    assert_eq!(ports.len(), 2);
    assert_ne!(ports[0], ports[1]);
    let (mesh, ollama) = (
        format!("http://127.0.0.1:{}", ports[0]),
        format!("http://127.0.0.1:{}", ports[1]),
    );

    assert_eq!(
        chat(&ollama).await.status(),
        200,
        "ollama port serves the local ollama"
    );
    assert_eq!(
        chat(&mesh).await.status(),
        404,
        "tokenshare port has no models of its own"
    );

    running.stop(Duration::from_secs(5)).await.unwrap();
    for url in [&mesh, &ollama] {
        assert!(
            reqwest::get(format!("{url}/_ts/status")).await.is_err(),
            "{url} should be closed"
        );
    }
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

#[tokio::test]
async fn the_configured_ollama_port_is_the_one_that_gets_bound() {
    let mock = mock_ollama(&["gemma2:latest"], 0).await;
    let (port, ollama_port) = (free_port(), free_port());
    let config = Config {
        port,
        masquerade: true,
        ollama_port,
        ollama_hosts: vec![mock.url.clone()],
        ..Config::default()
    };
    let dir = std::env::temp_dir().join(format!("ts-oport-{}", common::uuid_like()));
    let running = daemon::start(config, dir).await.unwrap();
    assert_eq!(running.ports(), vec![port, ollama_port]);

    assert_eq!(
        chat(&format!("http://127.0.0.1:{ollama_port}"))
            .await
            .status(),
        200
    );
    assert_eq!(
        chat(&format!("http://127.0.0.1:{port}")).await.status(),
        404
    );
    running.stop(Duration::from_secs(5)).await.unwrap();
}

#[tokio::test]
async fn a_busy_ollama_port_is_reported_and_nothing_is_left_half_started() {
    let mock = mock_ollama(&["gemma2:latest"], 0).await;
    let blocker = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    let busy = blocker.local_addr().unwrap().port();
    let port = free_port();
    let config = Config {
        port,
        masquerade: true,
        ollama_port: busy,
        ollama_hosts: vec![mock.url.clone()],
        ..Config::default()
    };
    let dir = std::env::temp_dir().join(format!("ts-busy-{}", common::uuid_like()));
    let err = daemon::start(config, dir).await.err().expect("must fail");
    assert!(
        err.to_string()
            .contains(&format!("port {busy} is already in use")),
        "{err:#}"
    );
    // The tokenshare port was not left bound by the failed start.
    assert!(!tokenshare_core::config::port_in_use(port));
}
