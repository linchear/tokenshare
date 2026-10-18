use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokenshare_core::mesh::rejoin;
use tokenshare_core::table::Status;

mod common;
use common::{eventually, node, Node, NodeOpts};

fn chat(model: &str) -> Value {
    json!({"model": model, "messages": []})
}

async fn post_chat(url: &str, model: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{url}/api/chat"))
        .json(&chat(model))
        .send()
        .await
        .unwrap()
}

/// A starts the mesh; B and C join through A.
async fn three_nodes() -> (Node, Node, Node) {
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        ..Default::default()
    })
    .await;
    let b = node(NodeOpts {
        models: &["llama3.2:latest"],
        fc: Some(&a.url),
        ..Default::default()
    })
    .await;
    let c = node(NodeOpts {
        fc: Some(&a.url),
        ..Default::default()
    })
    .await;
    assert_eq!(rejoin(&b.state).await, Some(true));
    assert_eq!(rejoin(&c.state).await, Some(true));
    (a, b, c)
}

async fn converged(nodes: &[&Node]) {
    eventually(5, "all tables to agree", || {
        let h = nodes[0].state.delegates.hash();
        nodes
            .iter()
            .all(|n| n.state.delegates.count() == nodes.len() && n.state.delegates.hash() == h)
    })
    .await;
}

#[tokio::test]
async fn join_announces_new_members_to_everyone() {
    let (a, b, c) = three_nodes().await;
    converged(&[&a, &b, &c]).await;
    // Everyone learned the models and URLs of everyone else.
    let peers = b.state.delegates.peers();
    assert!(peers
        .iter()
        .any(|p| p.url == a.url && p.models == vec!["gemma2:latest".to_string()]));
    assert!(peers.iter().any(|p| p.url == c.url));
    // A learned its own address from how B reached it.
    assert_eq!(a.state.delegates.self_entry().url, a.url);
}

#[tokio::test]
async fn request_is_routed_to_a_peer_that_has_the_model() {
    let (a, b, c) = three_nodes().await;
    converged(&[&a, &b, &c]).await;

    // C has no models; gemma2 lives on A.
    let resp = post_chat(&c.url, "gemma2").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap().lines().count(), 3);
    // It consumed no slot on C.
    assert_eq!(c.state.pool.in_use(), 0);

    // llama3.2 is served locally by B.
    assert_eq!(post_chat(&b.url, "llama3.2").await.status(), 200);
    // Nobody has this one.
    assert_eq!(post_chat(&c.url, "nope").await.status(), 404);
}

#[tokio::test]
async fn tags_is_the_union_across_the_pool() {
    let (a, b, c) = three_nodes().await;
    converged(&[&a, &b, &c]).await;
    let body: Value = reqwest::get(format!("{}/api/tags", c.url))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut names: Vec<_> = body["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap().to_string())
        .collect();
    names.sort();
    assert_eq!(names, vec!["gemma2:latest", "llama3.2:latest"]);
}

#[tokio::test]
async fn forwarded_requests_are_never_forwarded_again() {
    let (a, b, c) = three_nodes().await;
    converged(&[&a, &b, &c]).await;
    // B lacks gemma2. Marked as already handled by A, it must not go to A (or C).
    let resp = reqwest::Client::new()
        .post(format!("{}/api/chat", b.url))
        .header("x-tokenshare-visited", a.state.id.clone())
        .json(&chat("gemma2"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn full_peer_makes_us_try_the_next_peer() {
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        pool: 1,
        chunk_delay_ms: 150,
        ..Default::default()
    })
    .await;
    let d = node(NodeOpts {
        models: &["gemma2:latest"],
        fc: Some(&a.url),
        ..Default::default()
    })
    .await;
    let c = node(NodeOpts {
        fc: Some(&a.url),
        ..Default::default()
    })
    .await;
    for n in [&d, &c] {
        assert_eq!(rejoin(&n.state).await, Some(true));
    }
    converged(&[&a, &d, &c]).await;

    // Occupy A's only slot with a stream we don't finish reading.
    let held = post_chat(&a.url, "gemma2").await;
    assert_eq!(held.status(), 200);
    assert_eq!(a.state.pool.in_use(), 1);

    // C picks A or D at random; either way the request must succeed.
    for _ in 0..6 {
        assert_eq!(
            post_chat(&c.url, "gemma2")
                .await
                .text()
                .await
                .unwrap()
                .lines()
                .count(),
            3
        );
    }
    drop(held);
}

#[tokio::test]
async fn all_peers_full_is_503() {
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        pool: 1,
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
    converged(&[&a, &c]).await;

    let held = post_chat(&a.url, "gemma2").await;
    let resp = post_chat(&c.url, "gemma2").await;
    assert_eq!(resp.status(), 503);
    assert!(resp.headers().contains_key("retry-after"));
    drop(held);
}

#[tokio::test]
async fn forwarded_response_streams_chunk_by_chunk() {
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        chunk_delay_ms: 300,
        ..Default::default()
    })
    .await;
    let c = node(NodeOpts {
        fc: Some(&a.url),
        ..Default::default()
    })
    .await;
    assert_eq!(rejoin(&c.state).await, Some(true));
    converged(&[&a, &c]).await;

    let start = Instant::now();
    let mut resp = post_chat(&c.url, "gemma2").await;
    let first = resp.chunk().await.unwrap().unwrap();
    let first_at = start.elapsed();
    assert!(first.starts_with(b"{"));
    // The whole generation takes ~900ms; the first token must not wait for it.
    assert!(
        first_at < Duration::from_millis(700),
        "first chunk took {first_at:?}"
    );
    while resp.chunk().await.unwrap().is_some() {}
    assert!(start.elapsed() >= Duration::from_millis(850));
}

#[tokio::test]
async fn upstream_dying_mid_stream_truncates_the_client_stream() {
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        ..Default::default()
    })
    .await;
    let c = node(NodeOpts {
        fc: Some(&a.url),
        ..Default::default()
    })
    .await;
    assert_eq!(rejoin(&c.state).await, Some(true));
    converged(&[&a, &c]).await;

    let generate = |url: &str| {
        let url = format!("{url}/api/generate");
        async move {
            reqwest::Client::new()
                .post(url)
                .json(&chat("gemma2"))
                .send()
                .await
                .unwrap()
        }
    };
    // Locally on A, and forwarded through C: the client sees an error, not a clean end.
    for url in [&a.url, &c.url] {
        let resp = generate(url).await;
        assert_eq!(resp.status(), 200);
        assert!(
            resp.bytes().await.is_err(),
            "stream through {url} ended cleanly"
        );
    }
}

#[tokio::test]
async fn client_disconnect_cancels_the_forwarded_upstream_request() {
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        chunk_delay_ms: 200,
        ..Default::default()
    })
    .await;
    let c = node(NodeOpts {
        fc: Some(&a.url),
        ..Default::default()
    })
    .await;
    assert_eq!(rejoin(&c.state).await, Some(true));
    converged(&[&a, &c]).await;

    let mut resp = post_chat(&c.url, "gemma2").await;
    resp.chunk().await.unwrap().unwrap();
    assert!(!a.mock.chat_dropped.load(Ordering::SeqCst));
    drop(resp);
    let flag = a.mock.chat_dropped.clone();
    eventually(3, "ollama to see the cancel", || {
        flag.load(Ordering::SeqCst)
    })
    .await;
    eventually(3, "A's pool slot to free", || a.state.pool.in_use() == 0).await;
}

#[tokio::test]
async fn restart_rejoins_through_persisted_table_and_marks_dead_peers_inactive() {
    let dir = std::env::temp_dir().join(format!("ts-mesh-{}", common::uuid_like()));
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        ..Default::default()
    })
    .await;
    let b = node(NodeOpts {
        fc: Some(&a.url),
        data_dir: Some(dir.clone()),
        id: Some("b-fixed-id".into()),
        ..Default::default()
    })
    .await;
    assert_eq!(rejoin(&b.state).await, Some(true));
    assert_eq!(b.state.delegates.peers().len(), 1);

    // "Restart" B: a new process state reading the same data dir, with a dead
    // peer in its table and no fc configured.
    let dead = "http://127.0.0.1:1".to_string();
    {
        let t = tokenshare_core::table::DelegateTable::new(
            "b-fixed-id".into(),
            Some(b.url.clone()),
            Some(dir.join("table.json")),
        );
        let mut e =
            tokenshare_core::table::DelegateEntry::new("dead-peer".into(), dead.clone(), vec![]);
        e.last_seen += 1;
        t.merge([e]);
    }
    let config = tokenshare_core::Config {
        port: 0,
        ollama_hosts: vec![],
        ..Default::default()
    };
    let b2 = tokenshare_core::AppState::new("b-fixed-id".into(), config, Some(dir.clone()));
    assert_eq!(b2.delegates.peers().len(), 2, "table was loaded from disk");

    // Candidates are tried in random order; the dead one is never joined and
    // ends up inactive unless A happened to answer first, so retry until seen.
    let mut saw_inactive = false;
    for _ in 0..20 {
        assert_eq!(rejoin(&b2).await, Some(true));
        if b2
            .delegates
            .peers()
            .iter()
            .any(|p| p.url == dead && p.status == Status::Inactive)
        {
            saw_inactive = true;
            break;
        }
    }
    assert!(saw_inactive);
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn rejoin_with_nobody_reachable_reports_failure_and_nobody_known_reports_none() {
    let config = tokenshare_core::Config {
        fc: Some("http://127.0.0.1:1".into()),
        ..Default::default()
    };
    let s = tokenshare_core::AppState::new("lonely".into(), config, None);
    assert_eq!(rejoin(&s).await, Some(false));

    let s =
        tokenshare_core::AppState::new("alone".into(), tokenshare_core::Config::default(), None);
    assert_eq!(rejoin(&s).await, None);
}
