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

// ---- M3: keepalive gossip and table sync ----

use tokenshare_core::mesh::{send_keepalive, sync_once};

fn status_of(n: &Node, of: &Node) -> Status {
    n.state
        .delegates
        .peers()
        .into_iter()
        .find(|p| p.id == of.state.id)
        .unwrap()
        .status
}

fn models_of(n: &Node, of: &Node) -> Vec<String> {
    n.state
        .delegates
        .peers()
        .into_iter()
        .find(|p| p.id == of.state.id)
        .unwrap()
        .models
}

#[tokio::test]
async fn keepalive_updates_the_entry_and_is_relayed_to_everyone() {
    let (a, b, c) = three_nodes().await;
    converged(&[&a, &b, &c]).await;

    b.state
        .delegates
        .update_self(vec!["brand-new:latest".into()]);
    assert!(send_keepalive(&b.state).await);
    // B -> A or C; whoever got it relays to the other (never back to B).
    eventually(3, "A and C to learn B's models", || {
        models_of(&a, &b) == vec!["brand-new:latest".to_string()]
            && models_of(&c, &b) == vec!["brand-new:latest".to_string()]
    })
    .await;
}

#[tokio::test]
async fn duplicate_keepalives_are_dropped() {
    let (a, b, c) = three_nodes().await;
    converged(&[&a, &b, &c]).await;
    let msg = json!({
        "msg_id": "m-1", "via": b.state.id, "ttl": 3, "entry": b.state.delegates.self_entry(),
    });
    let post = || async {
        reqwest::Client::new()
            .post(format!("{}/_ts/keepalive", a.url))
            .json(&msg)
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()
    };
    assert_eq!(post().await["duplicate"], Value::Null);
    assert_eq!(post().await["duplicate"], true);
}

#[tokio::test]
async fn unresponsive_peer_is_marked_unavailable_and_another_is_tried() {
    let (a, b, mut c) = three_nodes().await;
    converged(&[&a, &b, &c]).await;
    c.kill().await;

    // Each call picks a random peer; if it picks C it must fall back to A.
    for _ in 0..30 {
        assert!(
            send_keepalive(&b.state).await,
            "keepalive must still reach A"
        );
        if status_of(&b, &c) == Status::Unavailable {
            break;
        }
    }
    assert_eq!(status_of(&b, &c), Status::Unavailable);
    assert_eq!(status_of(&b, &a), Status::Online);
}

#[tokio::test]
async fn unavailable_peer_is_skipped_by_routing() {
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        ..Default::default()
    })
    .await;
    let mut d = node(NodeOpts {
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
    d.kill().await;
    // Whichever of A or D C tries first, the request succeeds; D is marked
    // unavailable the first time it refuses a connection.
    for _ in 0..10 {
        assert_eq!(post_chat(&c.url, "gemma2").await.status(), 200);
    }
    for _ in 0..30 {
        if status_of(&c, &d) == Status::Unavailable {
            break;
        }
        assert_eq!(post_chat(&c.url, "gemma2").await.status(), 200);
    }
    assert_eq!(status_of(&c, &d), Status::Unavailable);
}

#[tokio::test]
async fn peer_comes_back_only_through_its_own_keepalive_not_through_sync() {
    let (a, b, mut c) = three_nodes().await;
    converged(&[&a, &b, &c]).await;
    c.kill().await;
    for _ in 0..30 {
        send_keepalive(&a.state).await;
        if status_of(&a, &c) == Status::Unavailable {
            break;
        }
    }
    assert_eq!(status_of(&a, &c), Status::Unavailable);

    c.restart().await;
    // Pulling tables (B still shows C online) must not resurrect it.
    for _ in 0..5 {
        assert!(sync_once(&a.state).await);
    }
    assert_eq!(status_of(&a, &c), Status::Unavailable);

    // C's own keepalive does. C picks A or B; B relays on to A.
    send_keepalive(&c.state).await;
    eventually(3, "A to see C online again", || {
        status_of(&a, &c) == Status::Online
    })
    .await;
}

#[tokio::test]
async fn sync_repairs_a_missed_announce() {
    let (a, mut b, c) = three_nodes().await;
    converged(&[&a, &b, &c]).await;

    // D joins via A while B is down, so B never hears the announcement.
    b.kill().await;
    let d = node(NodeOpts {
        models: &["qwen:latest"],
        fc: Some(&a.url),
        ..Default::default()
    })
    .await;
    assert_eq!(rejoin(&d.state).await, Some(true));
    eventually(3, "A and C to learn D", || {
        a.state.delegates.count() == 4 && c.state.delegates.count() == 4
    })
    .await;
    assert_eq!(b.state.delegates.count(), 3);

    b.restart().await;
    assert!(sync_once(&b.state).await);
    assert_eq!(b.state.delegates.count(), 4);
    assert_eq!(b.state.delegates.hash(), a.state.delegates.hash());
}

#[tokio::test]
async fn sync_with_everyone_down_reports_failure() {
    let (a, mut b, mut c) = three_nodes().await;
    converged(&[&a, &b, &c]).await;
    b.kill().await;
    c.kill().await;
    assert!(!sync_once(&a.state).await);
    assert_eq!(status_of(&a, &b), Status::Unavailable);
    assert_eq!(status_of(&a, &c), Status::Unavailable);
    // With nobody online, an isolated delegate still reaches out to try again.
    c.restart().await;
    assert!(sync_once(&a.state).await);
}

#[tokio::test]
async fn keepalive_loop_runs_on_its_interval() {
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        ..Default::default()
    })
    .await;
    let c = node(NodeOpts {
        fc: Some(&a.url),
        keepalive_interval_secs: Some(1),
        ..Default::default()
    })
    .await;
    assert_eq!(rejoin(&c.state).await, Some(true));
    converged(&[&a, &c]).await;
    let before = a.state.delegates.peers()[0].last_seen;

    // C's interval is 1s; the loop skips its immediate first tick.
    let handle = tokio::spawn(tokenshare_core::mesh::keepalive_loop(c.state.clone()));
    eventually(4, "A to receive a keepalive from C's loop", || {
        a.state.delegates.peers()[0].last_seen > before
    })
    .await;
    handle.abort();
}

// ---- not sharing with peers ----

#[tokio::test]
async fn a_delegate_that_does_not_share_advertises_no_models() {
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        share: false,
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

    assert!(models_of(&c, &a).is_empty(), "peers see no models");
    // Nobody else has the model, so as far as C is concerned it does not exist.
    assert_eq!(post_chat(&c.url, "gemma2").await.status(), 404);
    let tags: Value = reqwest::get(format!("{}/api/tags", c.url))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(tags["models"].as_array().unwrap().is_empty());
    // A's own users are unaffected.
    assert_eq!(post_chat(&a.url, "gemma2").await.status(), 200);
    let status: Value = reqwest::get(format!("{}/_ts/status", a.url))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["sharing"], false);
    assert_eq!(status["state"], "active", "its own ollama is still up");
}

#[tokio::test]
async fn forwarded_requests_are_refused_even_if_a_peer_has_stale_info() {
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        share: false,
        ..Default::default()
    })
    .await;
    // A peer that still believes A serves gemma2 sends it along.
    let resp = reqwest::Client::new()
        .post(format!("{}/api/chat", a.url))
        .header("x-tokenshare-visited", "some-peer")
        .json(&chat("gemma2"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
    assert_eq!(a.state.pool.in_use(), 0, "no slot was used");
}

#[tokio::test]
async fn a_non_sharing_delegate_still_uses_the_pool() {
    let d = node(NodeOpts {
        models: &["gemma2:latest"],
        ..Default::default()
    })
    .await;
    let a = node(NodeOpts {
        share: false,
        fc: Some(&d.url),
        ..Default::default()
    })
    .await;
    assert_eq!(rejoin(&a.state).await, Some(true));
    converged(&[&d, &a]).await;
    assert_eq!(post_chat(&a.url, "gemma2").await.status(), 200);
}

#[tokio::test]
async fn a_sharing_delegate_is_still_used_by_others() {
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
    assert_eq!(models_of(&c, &a), vec!["gemma2:latest".to_string()]);
    assert_eq!(post_chat(&c.url, "gemma2").await.status(), 200);
}

// ---- client-only delegates (no local ollama) ----

#[tokio::test]
async fn a_client_only_delegate_borrows_the_pools_ollama() {
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        ..Default::default()
    })
    .await;
    let c = node(NodeOpts {
        client_only: true,
        fc: Some(&a.url),
        ..Default::default()
    })
    .await;
    assert_eq!(rejoin(&c.state).await, Some(true));
    converged(&[&a, &c]).await;

    // Everything is served by A; C uses no slot and advertises nothing.
    let resp = post_chat(&c.url, "gemma2").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap().lines().count(), 3);
    assert_eq!(c.state.pool.in_use(), 0);
    assert!(models_of(&a, &c).is_empty());
    assert_eq!(post_chat(&c.url, "nope").await.status(), 404);

    // It can list the pool's models, but local-only endpoints have nowhere to go.
    let tags: Value = reqwest::get(format!("{}/api/tags", c.url))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(tags["models"][0]["name"], "gemma2:latest");
    assert_eq!(
        reqwest::get(format!("{}/api/ps", c.url))
            .await
            .unwrap()
            .status(),
        503
    );

    let status: Value = reqwest::get(format!("{}/_ts/status", c.url))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["state"], "active");
    assert_eq!(status["ollama"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn a_client_only_delegate_with_nobody_to_talk_to_is_down() {
    let c = node(NodeOpts {
        client_only: true,
        ..Default::default()
    })
    .await;
    let status: Value = reqwest::get(format!("{}/_ts/status", c.url))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["state"], "down");
    assert_eq!(post_chat(&c.url, "gemma2").await.status(), 404);
}

// ---- what ollama clients check first ----

#[tokio::test]
async fn a_delegate_with_no_local_ollama_still_answers_the_health_check() {
    let c = node(NodeOpts {
        client_only: true,
        ..Default::default()
    })
    .await;
    let client = reqwest::Client::new();
    // `ollama ls` starts with HEAD /; a failure makes the CLI try to launch Ollama.app.
    let head = client.head(format!("{}/", c.url)).send().await.unwrap();
    assert_eq!(head.status(), 200);
    let get = reqwest::get(format!("{}/", c.url)).await.unwrap();
    assert_eq!(get.status(), 200);
    assert_eq!(get.text().await.unwrap(), "Ollama is running");
    let v: Value = reqwest::get(format!("{}/api/version", c.url))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(v["version"].as_str().is_some_and(|s| !s.is_empty()));
}

#[tokio::test]
async fn tags_entries_for_peer_models_are_complete() {
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        ..Default::default()
    })
    .await;
    let c = node(NodeOpts {
        client_only: true,
        fc: Some(&a.url),
        ..Default::default()
    })
    .await;
    assert_eq!(rejoin(&c.state).await, Some(true));
    converged(&[&a, &c]).await;
    let tags: Value = reqwest::get(format!("{}/api/tags", c.url))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let e = &tags["models"][0];
    assert_eq!(e["name"], "gemma2:latest");
    // `ollama ls` slices the first 12 characters of the digest and panics without one.
    assert!(e["digest"].as_str().unwrap().len() >= 12);
    assert!(e["modified_at"].as_str().unwrap().ends_with('Z'));
    assert!(e["size"].is_number() && e["details"].is_object());
}

#[tokio::test]
async fn the_mesh_port_of_a_masquerading_delegate_also_answers_what_clients_check_first() {
    use tokenshare_core::config::Face;
    // A delegate with a working local ollama, served as the mesh-only face.
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        ..Default::default()
    })
    .await;
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(tokenshare_core::serve_face(
        l,
        a.state.clone(),
        Face::Mesh,
        std::future::pending(),
    ));

    let client = reqwest::Client::new();
    assert_eq!(
        client
            .head(format!("{url}/"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        reqwest::get(format!("{url}/"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "Ollama is running"
    );
    let v: Value = reqwest::get(format!("{url}/api/version"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_ne!(
        v["version"], "mock",
        "direct clients on the mesh port do not get the local ollama"
    );
    assert!(v["version"].as_str().is_some_and(|s| !s.is_empty()));
}
