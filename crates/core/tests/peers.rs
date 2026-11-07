//! Delegate names, limits on what a donor lends to its peers, and managing
//! peers from this machine.

use reqwest::StatusCode;
use serde_json::{json, Value};
use tokenshare_core::mesh::{pull_table, rejoin, send_keepalive};
use tokenshare_core::proxy::{SERVED_BY_HEADER, VISITED_HEADER};

mod common;
use common::{eventually, node, Node, NodeOpts};

async fn chat(url: &str, model: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{url}/api/chat"))
        .json(&json!({"model": model, "messages": []}))
        .send()
        .await
        .unwrap()
}

/// A request as another delegate would forward it to `url`.
async fn forwarded_chat(url: &str, model: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{url}/api/chat"))
        .header(VISITED_HEADER, "some-other-delegate")
        .json(&json!({"model": model, "messages": []}))
        .send()
        .await
        .unwrap()
}

async fn status(url: &str) -> Value {
    reqwest::get(format!("{url}/_ts/status"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// A client-only delegate joined to `donor`, with the donor's table pulled.
async fn client_of(donor: &Node) -> Node {
    let c = node(NodeOpts {
        fc: Some(&donor.url),
        client_only: true,
        name: Some("client"),
        ..Default::default()
    })
    .await;
    assert_eq!(rejoin(&c.state).await, Some(true));
    donor.state.refresh_local().await;
    pull_table(&c.state, &donor.url).await.unwrap();
    c
}

#[tokio::test]
async fn names_travel_with_joins_tables_and_keepalives() {
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        name: Some("gpu-box"),
        ..Default::default()
    })
    .await;
    let b = node(NodeOpts {
        fc: Some(&a.url),
        name: Some("  Living room\nMac  "),
        ..Default::default()
    })
    .await;
    assert_eq!(rejoin(&b.state).await, Some(true));

    // B learned A's name from the join reply; A learned B's from the join.
    assert_eq!(
        b.state.delegates.peers()[0].name.as_deref(),
        Some("gpu-box")
    );
    eventually(3, "A to list B", || a.state.delegates.peers().len() == 1).await;
    assert_eq!(
        a.state.delegates.peers()[0].name.as_deref(),
        Some("Living room Mac"),
        "cleaned before it was sent"
    );

    // A rename reaches peers with the next keepalive.
    // Only a newer `last_seen` wins, and on loopback all of this can happen
    // within one millisecond.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    b.state.delegates.set_self_name(Some("den".into()));
    assert!(send_keepalive(&b.state).await);
    assert_eq!(a.state.delegates.peers()[0].name.as_deref(), Some("den"));

    let s = status(&a.url).await;
    assert_eq!(s["name"], "gpu-box");
    let names: Vec<&str> = s["delegates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    assert_eq!(names.len(), 2);
    assert!(names.contains(&"gpu-box") && names.contains(&"den"));

    let metrics = reqwest::get(format!("{}/_ts/metrics", a.url))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(metrics.contains("name=\"gpu-box\""), "{metrics}");
    assert!(metrics.contains(&format!(
        "tokenshare_peer_info{{peer=\"{}\",id=\"{}\",name=\"den\",",
        b.url, b.state.id
    )));
}

#[tokio::test]
async fn peers_get_at_most_their_share_of_the_slots() {
    // Slow chunks keep each request (and its slot) busy while we look.
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        pool: 3,
        max_peer_requests: Some(1),
        chunk_delay_ms: 1500,
        ..Default::default()
    })
    .await;
    let c = client_of(&a).await;
    let peer_view = c.state.delegates.peers()[0].clone();
    assert_eq!(
        (peer_view.slots, peer_view.busy),
        (Some(1), Some(0)),
        "peers are told about the slots they may use"
    );

    // The first request from a peer gets the one peer slot...
    let first = chat(&c.url, "gemma2").await;
    assert_eq!(first.status(), StatusCode::OK);
    // ...and the second is turned away, though the pool has room.
    let second = chat(&c.url, "gemma2").await;
    assert_eq!(second.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(a.mock.chat_count(), 1);

    // This machine's own users still get the rest of the pool.
    let local = chat(&a.url, "gemma2").await;
    assert_eq!(local.status(), StatusCode::OK);
    assert_eq!(a.mock.chat_count(), 2);

    let s = status(&a.url).await;
    assert_eq!(s["pool"]["in_use"], 2);
    assert_eq!(s["pool"]["peer_limit"], 1);
    assert_eq!(s["pool"]["peer_in_use"], 1);
    let me = s["delegates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == a.state.id.as_str())
        .unwrap()
        .clone();
    assert_eq!(
        (&me["slots"], &me["busy"]),
        (&json!(1), &json!(1)),
        "full to peers"
    );

    // The peer slot frees with the response.
    first.text().await.unwrap();
    eventually(5, "the peer slot to free", || {
        a.state.pool.peer_in_use() == 0
    })
    .await;
    let third = chat(&c.url, "gemma2").await;
    assert_eq!(third.status(), StatusCode::OK);
    drop((local, third));
}

#[tokio::test]
async fn only_shared_models_are_offered_and_served_to_peers() {
    let a = node(NodeOpts {
        models: &["gemma2:latest", "llama3:latest", "llama3:70b"],
        shared_models: &["gemma2", "llama3:70b"],
        ..Default::default()
    })
    .await;
    let me = a.state.delegates.self_entry();
    assert_eq!(me.models, vec!["gemma2:latest", "llama3:70b"]);

    let c = client_of(&a).await;
    let r = chat(&c.url, "gemma2").await;
    assert_eq!(r.status(), StatusCode::OK);
    r.text().await.unwrap();
    // The private model is not in the pool for C at all.
    let r = chat(&c.url, "llama3").await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);

    // A peer with a stale view that asks anyway is refused, like a busy peer.
    let r = forwarded_chat(&a.url, "llama3").await;
    assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = r.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("does not share"));
    let r = forwarded_chat(&a.url, "llama3:70b").await;
    assert_eq!(r.status(), StatusCode::OK);
    r.text().await.unwrap();

    // This machine's own users get every model.
    let r = chat(&a.url, "llama3").await;
    assert_eq!(r.status(), StatusCode::OK);
    r.text().await.unwrap();
    assert_eq!(a.mock.chat_count(), 3);
    assert_eq!(
        status(&a.url).await["shared_models"],
        json!(["gemma2", "llama3:70b"])
    );
}

#[tokio::test]
async fn the_dashboard_may_forget_peers_but_other_web_pages_may_not() {
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        ..Default::default()
    })
    .await;
    let c = client_of(&a).await;
    eventually(3, "A to list C", || a.state.delegates.peers().len() == 1).await;
    assert_eq!(
        status(&a.url).await["manage"],
        true,
        "this machine may manage"
    );

    let forget = |origin: Option<String>| {
        let mut rb = reqwest::Client::new()
            .post(format!("{}/_ts/forget", a.url))
            .json(&json!({ "id": c.state.id }));
        if let Some(o) = origin {
            rb = rb.header("origin", o);
        }
        rb.send()
    };
    let r = forget(Some("http://evil.example".into())).await.unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    assert_eq!(a.state.delegates.peers().len(), 1, "still there");

    // The dashboard's own page, served by A.
    let r = forget(Some(a.url.clone())).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.json::<Value>().await.unwrap()["forgotten"], true);
    assert!(a.state.delegates.peers().is_empty());
}

#[tokio::test]
async fn the_dashboard_page_has_the_forget_controls() {
    let a = node(NodeOpts::default()).await;
    let page = reqwest::get(format!("{}/_ts/dashboard", a.url))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("/_ts/forget"));
    assert!(page.contains("data-forget"));
}

#[tokio::test]
async fn responses_say_which_machine_served_them() {
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        name: Some("Zoë's 100% box"),
        ..Default::default()
    })
    .await;
    let c = client_of(&a).await;
    let served_by =
        |r: &reqwest::Response| r.headers()[SERVED_BY_HEADER].to_str().unwrap().to_string();
    // Non-ASCII and `%` are percent-encoded, so any client can read it.
    let want = "Zo%C3%AB's 100%25 box";

    let r = chat(&a.url, "gemma2").await;
    assert_eq!(served_by(&r), want, "served here");
    r.text().await.unwrap();
    let r = chat(&c.url, "gemma2").await;
    assert_eq!(served_by(&r), want, "served by the peer");
    r.text().await.unwrap();

    // A delegate without a name is identified by its id.
    a.state.delegates.set_self_name(None);
    let r = chat(&a.url, "gemma2").await;
    assert_eq!(served_by(&r), a.state.id);
    r.text().await.unwrap();

    // Errors that no ollama produced carry none.
    let r = chat(&c.url, "nobody-has-this").await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    assert!(r.headers().get(SERVED_BY_HEADER).is_none());
}

#[tokio::test]
async fn versions_travel_with_joins_and_tables() {
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        name: Some("gpu-box"),
        ..Default::default()
    })
    .await;
    let c = client_of(&a).await;
    let ours = env!("CARGO_PKG_VERSION");

    // C learned A's versions from the join reply...
    let seen = &c.state.delegates.peers()[0];
    assert_eq!(seen.version.as_deref(), Some(ours));
    assert_eq!(
        seen.ollama_version.as_deref(),
        Some(common::MOCK_OLLAMA_VERSION)
    );
    // ...and A learned C's from the join: no ollama, so no ollama version.
    eventually(3, "A to list C", || a.state.delegates.peers().len() == 1).await;
    let c_seen = &a.state.delegates.peers()[0];
    assert_eq!(c_seen.version.as_deref(), Some(ours));
    assert_eq!(c_seen.ollama_version, None);

    let s = status(&a.url).await;
    assert_eq!(s["ollama_version"], common::MOCK_OLLAMA_VERSION);
    assert_eq!(s["ollama"][0]["version"], common::MOCK_OLLAMA_VERSION);
    let metrics = reqwest::get(format!("{}/_ts/metrics", c.url))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains(&format!(
            "name=\"gpu-box\",version=\"{ours}\",ollama_version=\"{}\"}} 1",
            common::MOCK_OLLAMA_VERSION
        )),
        "{metrics}"
    );
}

#[tokio::test]
async fn lending_and_borrowing_are_tallied_on_both_sides() {
    use common::{MOCK_COMPLETION_TOKENS, MOCK_PROMPT_TOKENS};
    let dir = std::env::temp_dir().join(format!("ts-ledger-{}", uuid_like()));
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        data_dir: Some(dir.clone()),
        ..Default::default()
    })
    .await;
    let c = client_of(&a).await;
    eventually(3, "A to list C", || a.state.delegates.peers().len() == 1).await;
    for _ in 0..2 {
        let r = chat(&c.url, "gemma2").await;
        assert_eq!(r.status(), StatusCode::OK);
        r.text().await.unwrap();
    }
    // A's own user: nobody lent or borrowed anything.
    chat(&a.url, "gemma2").await.text().await.unwrap();
    let per_request = MOCK_PROMPT_TOKENS + MOCK_COMPLETION_TOKENS;

    eventually(3, "both sides to count", || {
        a.state.ledger.totals().lent.requests == 2 && c.state.ledger.totals().borrowed.requests == 2
    })
    .await;
    let lent = a.state.ledger.balances()[&c.state.id].lent;
    assert_eq!(lent.tokens(), 2 * per_request);
    assert_eq!(lent.prompt_tokens, 2 * MOCK_PROMPT_TOKENS);
    let borrowed = c.state.ledger.balances()[&a.state.id].borrowed;
    assert_eq!(borrowed.tokens(), 2 * per_request);
    assert_eq!(a.state.ledger.totals().borrowed.requests, 0);

    let s = status(&c.url).await;
    assert_eq!(s["ledger"]["totals"]["borrowed"]["requests"], 2);
    assert_eq!(
        s["ledger"]["peers"][&a.state.id]["borrowed"]["completion_tokens"],
        2 * MOCK_COMPLETION_TOKENS
    );
    let metrics = reqwest::get(format!("{}/_ts/metrics", a.url))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(metrics.contains(&format!(
        "tokenshare_lent_requests_total{{id=\"{}\"}} 2\n",
        c.state.id
    )));
    assert!(metrics.contains(&format!(
        "tokenshare_lent_tokens_total{{id=\"{}\",kind=\"completion\"}} {}\n",
        c.state.id,
        2 * MOCK_COMPLETION_TOKENS
    )));

    // Saved with the table, and read back by the next start.
    a.state.ledger.flush();
    let again = tokenshare_core::ledger::Ledger::load(Some(dir.join("ledger.json")));
    assert_eq!(again.balances()[&c.state.id].lent.requests, 2);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_forwarded_request_from_a_stranger_is_not_tallied() {
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        ..Default::default()
    })
    .await;
    let r = forwarded_chat(&a.url, "gemma2").await;
    assert_eq!(r.status(), StatusCode::OK);
    r.text().await.unwrap();
    assert!(a.state.ledger.balances().is_empty());
}

fn uuid_like() -> String {
    format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}
