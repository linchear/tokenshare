//! Load-aware routing: which peer a request is sent to, given what peers
//! publish (slots, busy, warm models) and what the sender sees first-hand.

use serde_json::{json, Value};
use tokenshare_core::mesh::{pull_table, rejoin};

mod common;
use common::{eventually, node, Node, NodeOpts};

async fn post_chat(url: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{url}/api/chat"))
        .json(&json!({"model": "gemma2", "messages": []}))
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

/// Two donors with the same model and a client-only delegate C that knows both.
async fn two_donors_and_a_client(a_opts: NodeOpts<'_>, b_opts: NodeOpts<'_>) -> (Node, Node, Node) {
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        ..a_opts
    })
    .await;
    let b = node(NodeOpts {
        models: &["gemma2:latest"],
        fc: Some(&a.url),
        ..b_opts
    })
    .await;
    let c = node(NodeOpts {
        fc: Some(&a.url),
        client_only: true,
        ..Default::default()
    })
    .await;
    assert_eq!(rejoin(&b.state).await, Some(true));
    assert_eq!(rejoin(&c.state).await, Some(true));
    eventually(5, "C to know both donors", || {
        c.state.delegates.peers().len() == 2
    })
    .await;
    (a, b, c)
}

/// Have C learn A's and B's current load straight from them, as a table pull
/// or keepalive would.
async fn refresh_view(a: &Node, b: &Node, c: &Node) {
    a.state.refresh_local().await;
    b.state.refresh_local().await;
    pull_table(&c.state, &a.url).await.unwrap();
    pull_table(&c.state, &b.url).await.unwrap();
}

#[tokio::test]
async fn a_peer_with_the_model_loaded_is_preferred() {
    let (a, b, c) = two_donors_and_a_client(NodeOpts::default(), NodeOpts::default()).await;
    a.mock.set_loaded(&[]);
    b.mock.set_loaded(&["gemma2:latest"]);
    refresh_view(&a, &b, &c).await;

    let b_seen = c
        .state
        .delegates
        .peers()
        .into_iter()
        .find(|p| p.id == b.state.id)
        .unwrap();
    assert_eq!(b_seen.loaded, vec!["gemma2:latest".to_string()]);
    assert_eq!(b_seen.slots, Some(3));

    for _ in 0..8 {
        let r = post_chat(&c.url).await;
        assert_eq!(r.status(), 200);
        r.text().await.unwrap();
    }
    assert_eq!(
        b.mock.chat_count(),
        8,
        "every request went to the warm peer"
    );
    assert_eq!(a.mock.chat_count(), 0);
}

#[tokio::test]
async fn a_peer_whose_slots_are_all_busy_is_tried_last() {
    let (a, b, c) = two_donors_and_a_client(
        NodeOpts::default(),
        NodeOpts {
            pool: 1,
            chunk_delay_ms: 400,
            ..Default::default()
        },
    )
    .await;
    // Both cold, so only capacity tells them apart. Fill B's only slot.
    a.mock.set_loaded(&[]);
    b.mock.set_loaded(&[]);
    let hog = post_chat(&b.url).await;
    assert_eq!(b.state.pool.in_use(), 1);
    refresh_view(&a, &b, &c).await;

    for _ in 0..5 {
        let r = post_chat(&c.url).await;
        assert_eq!(r.status(), 200);
        r.text().await.unwrap();
    }
    assert_eq!(a.mock.chat_count(), 5);
    assert_eq!(b.mock.chat_count(), 1, "only the request that filled it");
    let s = status(&c.url).await;
    let refused = s["activity"]["targets"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["declined"].as_u64().unwrap_or(0) > 0);
    assert!(!refused, "B was never asked, so it never had to say no");
    hog.text().await.unwrap();
}

#[tokio::test]
async fn concurrent_requests_spread_across_equal_peers() {
    let slow = || NodeOpts {
        chunk_delay_ms: 300,
        ..Default::default()
    };
    let (a, b, c) = two_donors_and_a_client(slow(), slow()).await;
    a.mock.set_loaded(&[]);
    b.mock.set_loaded(&[]);
    refresh_view(&a, &b, &c).await;

    // The first response is still streaming when the second request is routed,
    // so C knows it has one in flight to that peer and picks the other.
    let first = post_chat(&c.url).await;
    let second = post_chat(&c.url).await;
    assert_eq!((a.mock.chat_count(), b.mock.chat_count()), (1, 1));
    first.text().await.unwrap();
    second.text().await.unwrap();
}

#[tokio::test]
async fn a_peer_that_declines_is_cooled_down_and_the_request_still_succeeds() {
    let (a, b, c) = two_donors_and_a_client(
        NodeOpts::default(),
        NodeOpts {
            pool: 1,
            chunk_delay_ms: 400,
            ..Default::default()
        },
    )
    .await;
    // Make B the better choice on paper (warm), then fill it without C knowing.
    a.mock.set_loaded(&[]);
    b.mock.set_loaded(&["gemma2:latest"]);
    refresh_view(&a, &b, &c).await;
    let hog = post_chat(&b.url).await;

    let r = post_chat(&c.url).await;
    assert_eq!(r.status(), 200, "B said busy, so A served it");
    r.text().await.unwrap();
    assert_eq!(a.mock.chat_count(), 1);

    let s = status(&c.url).await;
    let t = s["activity"]["targets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["target"] == b.url.as_str())
        .unwrap()
        .clone();
    assert_eq!(t["declined"], 1);
    assert_eq!(t["cooling"], true);

    // While B cools down it goes after A, even though it looks warm.
    let r = post_chat(&c.url).await;
    r.text().await.unwrap();
    assert_eq!(a.mock.chat_count(), 2);
    assert_eq!(b.mock.chat_count(), 1, "B was not asked again");
    hog.text().await.unwrap();
}

#[tokio::test]
async fn a_local_host_with_the_model_loaded_is_preferred() {
    let cold = common::mock_ollama(&["gemma2:latest"], 0).await;
    let warm = common::mock_ollama(&["gemma2:latest"], 0).await;
    cold.set_loaded(&[]);
    warm.set_loaded(&["gemma2:latest"]);
    let config = tokenshare_core::Config {
        ollama_hosts: vec![cold.url.clone(), warm.url.clone()],
        ..Default::default()
    };
    let s = tokenshare_core::AppState::new(common::uuid_like(), config, None);
    s.refresh_local().await;
    assert_eq!(
        s.table.host_for_model("gemma2").as_deref(),
        Some(warm.url.as_str())
    );
    assert_eq!(
        s.delegates.self_entry().loaded,
        vec!["gemma2:latest".to_string()]
    );
    // Once it is no longer loaded, any host with the model will do.
    warm.set_loaded(&[]);
    s.refresh_local().await;
    assert_eq!(
        s.table.host_for_model("gemma2").as_deref(),
        Some(cold.url.as_str())
    );
}

#[tokio::test]
async fn a_delegate_that_does_not_share_publishes_no_load() {
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        share: false,
        ..Default::default()
    })
    .await;
    let me = a.state.delegates.self_entry();
    assert_eq!((me.slots, me.busy), (None, None));
    assert!(me.loaded.is_empty());
}

#[tokio::test]
async fn status_lists_recent_responses_and_the_dashboard_is_served() {
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        ..Default::default()
    })
    .await;
    post_chat(&a.url).await.text().await.unwrap();
    eventually(3, "the response to be recorded", || {
        a.state.activity.snapshot()["recent"]
            .as_array()
            .unwrap()
            .len()
            == 1
    })
    .await;
    let s = status(&a.url).await;
    let r = &s["activity"]["recent"][0];
    assert_eq!(r["model"], "gemma2");
    assert_eq!(r["route"], "local");
    assert_eq!(r["outcome"], "complete");
    assert_eq!(r["problem"], "");
    assert_eq!(s["activity"]["targets"][0]["ok"], 1);
    assert_eq!(s["activity"]["targets"][0]["inflight"], 0);
    assert_eq!(s["ollama"][0]["loaded"][0], "running-here");
    assert!(s["version"].is_string());

    let page = reqwest::get(format!("{}/_ts/dashboard", a.url))
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    assert!(page
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("text/html"));
    assert!(page.text().await.unwrap().contains("/_ts/status"));
}
